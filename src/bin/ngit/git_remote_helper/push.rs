use core::str::{self, FromStr};
use std::{
    collections::{HashMap, HashSet},
    io::Stdin,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bitcoin_hashes::sha1::Hash as Sha1Hash;
use client::{get_events_from_local_cache, get_issues_from_cache, get_state_from_cache};
use console::Term;
use git::{RepoActions, sha1_to_oid};
use git_events::{
    generate_cover_letter_and_patch_events, generate_patch_event, get_commit_id_from_patch,
};
use git2::Repository;
use ngit::{
    client::{self, Client, get_event_from_cache_by_id, get_filter_state_events},
    git::{self, Repo, nostr_url::NostrUrlDecoded},
    git_events::{
        self, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, event_to_cover_letter, get_event_root,
        get_status, sign_ordered_status_event, status_kinds, tag_value,
    },
    list::list_from_remotes,
    login::{SignerInfo, existing::load_existing_login, user::UserRef},
    output_mode::write_progress_line,
    proposal_base::{
        ProposalBaseInference, commits_after_base, infer_proposal_base,
        merge_base_for_fast_forward_update, resolve_explicit_base, resolve_target_branch_tip,
    },
    push::select_servers_push_refs_and_generate_pr_or_pr_update_event,
    repo_ref, repo_state,
    signer::NgitSigner,
    utils::{
        find_proposal_and_patches_by_branch_name, get_all_proposals, get_open_or_draft_proposals,
        get_remote_name_by_url, get_short_git_server_name, read_line,
    },
};
use nostr::prelude::{
    Event, EventBuilder, EventId, FromBech32, Kind, PublicKey, Tag,
    nip01::Nip01Tag,
    nip10::{Marker, Nip10Tag},
    nip19::{Nip19, ToBech32},
    nip22::CommentTarget,
    nip34::Nip34Tag,
};
use repo_ref::RepoRef;
use repo_state::RepoState;

use crate::state_transaction::{
    LiveOps, StateTransaction, StateTransactionFailure, publish_events_to_relays,
};

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
pub(super) async fn run_push(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    stdin: &Stdin,
    initial_refspec: &str,
    client: &mut Client,
    remote_name: Option<&str>,
    list_outputs: Option<super::list::ListResult>,
    title_description: Option<(String, String)>,
    git_server_push_options: Vec<String>,
    git_server: Option<String>,
    proposal_options: super::ProposalOptions,
    force_with_lease: &HashMap<String, Option<String>>,
    private_signer: Option<&Arc<NgitSigner>>,
    command_signer: Option<&SignerInfo>,
) -> Result<()> {
    let refspecs = get_refspecs_from_push_batch(stdin, initial_refspec)?;

    let mut proposal_refspecs = refspecs
        .iter()
        .filter(|r| r.contains("refs/heads/pr/"))
        .cloned()
        .collect::<Vec<String>>();

    let mut git_state_refspecs = refspecs
        .iter()
        .filter(|r| !r.contains("refs/heads/pr/"))
        .cloned()
        .collect::<Vec<String>>();

    let term = console::Term::stderr();

    let (list_outputs, advertised_refs) = if let Some(outputs) = list_outputs {
        (outputs.remote_states, outputs.advertised_refs)
    } else {
        (
            list_from_remotes(
                &term,
                git_repo,
                &repo_ref.git_server,
                &repo_ref.to_nostr_git_url(&None),
                None,
                if repo_ref.private {
                    private_signer
                } else {
                    None
                },
            )
            .await,
            HashMap::new(),
        )
    };

    apply_force_with_lease(
        &mut git_state_refspecs,
        &mut proposal_refspecs,
        force_with_lease,
        &advertised_refs,
    )?;

    let existing_state = {
        // if no state events - create from first git server listed
        if let Ok(nostr_state) = &get_state_from_cache(Some(git_repo.get_path()?), repo_ref).await {
            nostr_state.state.clone()
        } else if let Some(url) = repo_ref
            .git_server
            .iter()
            .find(|&url| list_outputs.contains_key(url))
        {
            let (state, _is_grasp_server) = list_outputs.get(url).unwrap().to_owned();
            state
        } else {
            bail!(
                "failed to connect to git servers: {}",
                repo_ref.git_server.join(" ")
            );
        }
    };

    let (rejected_refspecs, remote_refspecs) = create_rejected_refspecs_and_remotes_refspecs(
        &term,
        git_repo,
        &git_state_refspecs,
        &existing_state,
        &list_outputs,
    )?;

    git_state_refspecs.retain(|refspec| {
        if let Some(rejected) = rejected_refspecs.get(&refspec.clone()) {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            println!("error {to} {} out of sync with nostr", rejected.join(" "));
            false
        } else {
            true
        }
    });

    // all refspecs aren't rejected
    if !(git_state_refspecs.is_empty() && proposal_refspecs.is_empty()) {
        let PushEventsPlan {
            rejected_proposal_refspecs,
            rejected_git_server_refspecs,
            rejected,
            state,
            other_events,
            my_write_relays,
            repo_relay_only,
        } = create_events_and_proposals(
            git_repo,
            repo_ref,
            &git_state_refspecs,
            &proposal_refspecs,
            client, // &mut Client
            existing_state,
            &term,
            title_description.as_ref(),
            &git_server_push_options,
            git_server.as_deref(),
            &proposal_options,
            command_signer,
        )
        .await?;

        // Like the out-of-sync and stale-lease rejections above, refspecs
        // refused by the maintainer-listing check have received their
        // `error` responses and must not reach the state transaction.
        git_state_refspecs.retain(|refspec| !rejected_git_server_refspecs.contains(refspec));

        if !rejected {
            let decoded_nostr_url = repo_ref.to_nostr_git_url(&None);
            let mut ops = LiveOps {
                client,
                git_repo,
                term: &term,
                git_server_push_options: &git_server_push_options,
                decoded_nostr_url: &decoded_nostr_url,
            };

            if git_state_refspecs.is_empty() {
                if !other_events.is_empty() {
                    publish_events_to_relays(
                        &mut ops,
                        &repo_ref.relays,
                        other_events,
                        &my_write_relays,
                        repo_relay_only,
                        None,
                    )
                    .await?;
                }

                for refspec in git_state_refspecs.iter().chain(proposal_refspecs.iter()) {
                    if rejected_proposal_refspecs.contains(refspec) {
                        continue;
                    }
                    mark_refspec_pushed(git_repo, repo_ref, refspec, remote_name)?;
                }

                println!();
                return Ok(());
            }

            let mut transaction = StateTransaction::new(repo_ref, state);

            // The full phase sequence — GRASP staging, git pushes, relay
            // fanout and the cache commit point — runs inside the
            // transaction driver; see the state_transaction module docs
            // for the ordering rationale.
            let push_result = transaction
                .execute(
                    &mut ops,
                    remote_refspecs,
                    &git_state_refspecs,
                    &my_write_relays,
                    repo_relay_only,
                )
                .await?;

            // Proposal, status and announcement events are published
            // once a git server accepted the pushed data — including
            // when the state event subsequently reached no relay; a
            // failed git push publishes nothing.
            let git_data_accepted = matches!(
                push_result,
                Ok(()) | Err(StateTransactionFailure::StateNotAcceptedByAnyRelay)
            );

            if git_data_accepted {
                if !other_events.is_empty() {
                    publish_events_to_relays(
                        &mut ops,
                        &repo_ref.relays,
                        other_events,
                        &my_write_relays,
                        repo_relay_only,
                        None,
                    )
                    .await?;
                }
                // Proposal refspecs are reported `ok` only now that their
                // events have been published; reporting them before the
                // transaction would tell git a proposal exists on nostr
                // when no relay ever received its events.
                for refspec in &proposal_refspecs {
                    if rejected_proposal_refspecs.contains(refspec) {
                        continue;
                    }
                    mark_refspec_pushed(git_repo, repo_ref, refspec, remote_name)?;
                }
            }

            match push_result {
                Ok(()) => {
                    // The transaction committed: the accepted candidate
                    // is now the authoritative cached state.
                    for refspec in &git_state_refspecs {
                        mark_refspec_pushed(git_repo, repo_ref, refspec, remote_name)?;
                    }
                }
                Err(failure) => {
                    report_state_push_failure(&git_state_refspecs, &failure)?;
                    if !git_data_accepted {
                        report_proposal_push_failure(
                            &proposal_refspecs,
                            &rejected_proposal_refspecs,
                            &failure,
                        )?;
                    }
                }
            }
        }
    }

    println!();
    Ok(())
}

fn apply_force_with_lease(
    git_state_refspecs: &mut Vec<String>,
    proposal_refspecs: &mut Vec<String>,
    force_with_lease: &HashMap<String, Option<String>>,
    advertised_refs: &HashMap<String, String>,
) -> Result<()> {
    let pushed_targets = git_state_refspecs
        .iter()
        .chain(proposal_refspecs.iter())
        .map(|refspec| refspec_to_from_to(refspec).map(|(_, to)| to.to_string()))
        .collect::<Result<HashSet<_>>>()?;

    let stale_targets = force_with_lease
        .iter()
        .filter(|(ref_name, expected)| {
            pushed_targets.contains(*ref_name)
                && advertised_refs
                    .get(*ref_name)
                    .filter(|value| !value.starts_with("ref: "))
                    != expected.as_ref()
        })
        .map(|(ref_name, _)| ref_name.clone())
        .collect::<HashSet<_>>();

    for ref_name in &stale_targets {
        println!("error {ref_name} stale info");
    }
    git_state_refspecs.retain(|refspec| {
        refspec_to_from_to(refspec).is_ok_and(|(_, to)| !stale_targets.contains(to))
    });
    proposal_refspecs.retain(|refspec| {
        refspec_to_from_to(refspec).is_ok_and(|(_, to)| !stale_targets.contains(to))
    });
    for refspec in git_state_refspecs
        .iter_mut()
        .chain(proposal_refspecs.iter_mut())
    {
        let to = refspec_to_from_to(refspec)?.1.to_string();
        if force_with_lease.contains_key(&to) {
            *refspec = ensure_force_push_refspec(refspec);
        }
    }
    Ok(())
}

struct PushEventsPlan {
    rejected_proposal_refspecs: Vec<String>,
    /// refs/heads and refs/tags refspecs refused by the maintainer-listing
    /// check. Their `error` responses have already been written, so the
    /// caller must drop them before the state transaction — otherwise their
    /// git data would still be pushed and the same refs reported a second
    /// time.
    rejected_git_server_refspecs: Vec<String>,
    rejected: bool,
    /// the candidate replacement repository state (`None` under
    /// `nostr.nostate` or when no state refspecs are being pushed)
    state: Option<RepoState>,
    /// proposal, status and announcement events published alongside the
    /// state event
    other_events: Vec<Event>,
    my_write_relays: Vec<String>,
    repo_relay_only: bool,
}

fn mark_refspec_pushed(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    refspec: &str,
    remote_name: Option<&str>,
) -> Result<()> {
    let (_, to) = refspec_to_from_to(refspec)?;
    println!("ok {to}");
    self_heal_legacy_tag_tracking_ref(
        &git_repo.git_repo,
        refspec,
        remote_name,
        &repo_ref.to_nostr_git_url(&None).to_string(),
    )
    .context("could not remove legacy tag tracking ref")
}

/// Report per-ref `error` lines for a failed state push. The local cache
/// is deliberately left untouched: the candidate was never cached, so the
/// previously committed state remains authoritative.
fn report_state_push_failure(
    git_state_refspecs: &[String],
    failure: &StateTransactionFailure,
) -> Result<()> {
    for refspec in git_state_refspecs {
        let (_, to) = refspec_to_from_to(refspec)?;
        println!("error {to} {}", failure.user_message());
    }
    Ok(())
}

/// Report per-ref `error` lines for proposal refspecs whose events were
/// never published because no git server accepted the pushed data.
/// Refspecs rejected during event creation already printed their own
/// `error` lines and are skipped.
fn report_proposal_push_failure(
    proposal_refspecs: &[String],
    rejected_proposal_refspecs: &[String],
    failure: &StateTransactionFailure,
) -> Result<()> {
    for refspec in proposal_refspecs {
        if rejected_proposal_refspecs.contains(refspec) {
            continue;
        }
        let (_, to) = refspec_to_from_to(refspec)?;
        println!("error {to} {}", failure.user_message());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn create_events_and_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    git_server_refspecs: &Vec<String>,
    proposal_refspecs: &Vec<String>,
    client: &mut Client,
    existing_state: HashMap<String, String>,
    term: &Term,
    title_description: Option<&(String, String)>,
    git_server_push_options: &[String],
    git_server: Option<&str>,
    proposal_options: &super::ProposalOptions,
    command_signer: Option<&SignerInfo>,
) -> Result<PushEventsPlan> {
    let command_signer = command_signer.cloned();
    let (signer, mut user_ref, _) = load_existing_login(
        &Some(git_repo),
        &command_signer,
        &None,
        &None,
        Some(client),
        true,  // silent
        false, // prompt_for_password - MUST be false for non-interactive
        true,  // fetch_profile_updates
    )
    .await
    .context(
        "authentication required; run 'ngit account login' or 'ngit account create', then try again",
    )?;

    let authorized_maintainer = repo_ref.is_authorized_maintainer(&user_ref.public_key);
    if !authorized_maintainer {
        let rejection = if repo_ref
            .invited_maintainers()
            .contains(&user_ref.public_key)
        {
            "you are invited as a maintainer but have not accepted; run `ngit repo accept` first"
                .to_string()
        } else {
            format!(
                "your nostr account {} is not a confirmed maintainer of the repo",
                user_ref.metadata.name
            )
        };
        for refspec in git_server_refspecs {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            // `error <dst> <why>` is the remote-helper protocol response on
            // stdout — like the out-of-sync and stale-lease rejections — so
            // git reports the ref as rejected and exits non-zero. A stderr
            // message would leave git believing nothing needed pushing.
            println!("error {to} {rejection}");
        }
        if proposal_refspecs.is_empty() {
            return Ok(PushEventsPlan {
                rejected_proposal_refspecs: vec![],
                rejected_git_server_refspecs: git_server_refspecs.clone(),
                rejected: true,
                state: None,
                other_events: vec![],
                my_write_relays: vec![],
                repo_relay_only: false,
            });
        }
    }

    let mut events = vec![];
    let mut state: Option<RepoState> = None;
    // The nostr repo-state event's HEAD tag is the maintainer-declared default
    // branch — the most authoritative source for default-branch
    // identification when deciding whether commit-message issue keywords
    // should auto-resolve issues and when scoping proposal fork points.
    let declared_default_branch = repo_state::default_branch_from_state(&existing_state);

    // A rejected pusher's branch refspecs produce no state candidate, no
    // merge/issue status events and no maintainers.yaml update; only their
    // proposal refspecs are processed below.
    if authorized_maintainer && !git_server_refspecs.is_empty() {
        let new_state = generate_updated_state(git_repo, &existing_state, git_server_refspecs)?;

        let store_state =
            if let Ok(Some(nostate)) = git_repo.get_git_config_item("nostr.nostate", None) {
                !nostate.eq("true")
            } else {
                true
            };

        if store_state {
            // The latest cached state event is the NIP-01 ordering
            // reference for the candidate replacement.
            let old_state_event = get_events_from_local_cache(
                git_repo.get_path()?,
                vec![get_filter_state_events(&repo_ref.coordinates(), true)],
            )
            .await
            .ok()
            .and_then(|events| ngit::event_ordering::latest_event(&events).cloned());

            // The candidate is ordered after the cached predecessor here
            // but only cached by StateTransaction::commit once a git
            // server and a relay accepted it. Until then the predecessor
            // remains the NIP-01 ordering reference:
            // this helper process does not report success (or exit) before
            // the commit point, so a subsequent push orders from the
            // committed event, while a concurrent process ordering from
            // the predecessor is resolved by NIP-01 replacement ordering
            // exactly like two independent machines pushing at once.
            state = Some(
                RepoState::build(
                    repo_ref.identifier.clone(),
                    new_state,
                    &signer,
                    old_state_event.as_ref(),
                )
                .await?,
            );
        }

        let merge_status_context = MergeStatusContext {
            decoded_nostr_url: &repo_ref.to_nostr_git_url(&None),
            repo_ref,
            git_repo,
            signer: &signer,
            existing_state: &existing_state,
            declared_default_branch: declared_default_branch.as_deref(),
        };

        match get_merged_status_events(term, git_server_refspecs, merge_status_context).await {
            Ok(merged_status_events) => {
                for event in merged_status_events {
                    events.push(event);
                }
            }
            Err(err) => {
                term.write_line(
                    format!("warning: unable to build proposal merge status events: {err:#}")
                        .as_str(),
                )?;
            }
        }

        match get_issue_resolution_status_events(
            term,
            &repo_ref.to_nostr_git_url(&None),
            repo_ref,
            git_repo,
            &signer,
            git_server_refspecs,
            declared_default_branch.as_deref(),
        )
        .await
        {
            Ok(issue_resolution_events) => {
                for event in issue_resolution_events {
                    events.push(event);
                }
            }
            Err(err) => {
                term.write_line(
                    format!("warning: unable to build issue resolution status events: {err:#}")
                        .as_str(),
                )?;
            }
        }
    }

    let (proposal_events, rejected_proposal_refspecs) = process_proposal_refspecs(
        client,
        git_repo,
        repo_ref,
        proposal_refspecs,
        &mut user_ref,
        &signer,
        term,
        title_description,
        git_server_push_options,
        git_server,
        declared_default_branch.as_deref(),
        proposal_options,
    )
    .await?;
    for e in proposal_events {
        events.push(e);
    }

    // TODO check whether tip of each branch pushed is on at least one git server
    // before broadcasting the nostr state
    let repo_relay_only = repo_ref.private
        || git_repo
            .get_git_config_item("nostr.repo-relay-only", None)
            .ok()
            .flatten()
            .is_some_and(|value| value == "true");

    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        user_ref.relays.write()
    };

    Ok(PushEventsPlan {
        rejected_proposal_refspecs,
        rejected_git_server_refspecs: if authorized_maintainer {
            vec![]
        } else {
            git_server_refspecs.clone()
        },
        rejected: false,
        state,
        other_events: events,
        my_write_relays,
        repo_relay_only,
    })
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn process_proposal_refspecs(
    client: &Client,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    proposal_refspecs: &Vec<String>,
    user_ref: &mut UserRef,
    signer: &Arc<NgitSigner>,
    term: &Term,
    title_description: Option<&(String, String)>,
    git_server_push_options: &[String],
    git_server: Option<&str>,
    default_branch: Option<&str>,
    proposal_options: &super::ProposalOptions,
) -> Result<(Vec<Event>, Vec<String>)> {
    let mut events = vec![];
    let mut rejected_proposal_refspecs = vec![];
    if proposal_refspecs.is_empty() {
        return Ok((events, rejected_proposal_refspecs));
    }
    let all_proposals = get_all_proposals(git_repo, repo_ref).await?;
    let explicit_base = if let Some(reference) = &proposal_options.base {
        Some(resolve_explicit_base(git_repo, repo_ref, reference, &all_proposals).await?)
    } else {
        None
    };
    let open_proposals = if explicit_base.is_none() {
        get_open_or_draft_proposals(git_repo, repo_ref).await?
    } else {
        HashMap::new()
    };
    let current_user = user_ref.public_key;

    for refspec in proposal_refspecs {
        let (from, to) = refspec_to_from_to(refspec).unwrap();
        let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;

        // this failed to find existing PR from user
        if let Some((_, (proposal, patches, pr_upgrade_root))) =
            find_proposal_and_patches_by_branch_name(to, &all_proposals, Some(&current_user))
        {
            if proposal_options.target_branch.is_some() {
                bail!("target-branch can only be set when opening a new PR");
            }
            // After a patch→PR upgrade, pr_upgrade_root is the KIND_PULL_REQUEST
            // event that should be referenced as the root of any subsequent PR
            // updates (its E tag).  For normal PRs (proposal is already PR kind)
            // and plain patch threads there is no pr_upgrade_root so we fall back
            // to the proposal event itself.
            let effective_root: &Event = pr_upgrade_root.as_ref().unwrap_or(proposal);
            let inherited_target = tag_value(effective_root, "b").ok();
            let target_tips = if let Some(branch) = &inherited_target {
                vec![resolve_target_branch_tip(
                    git_repo,
                    branch,
                    default_branch,
                    false,
                )?]
            } else {
                git_repo.get_default_branch_tips(default_branch)?
            };
            let inference = if explicit_base.is_none() {
                infer_proposal_base(
                    git_repo,
                    repo_ref,
                    &open_proposals,
                    Some(proposal.id),
                    patches.first(),
                    proposal.pubkey,
                    &tip_of_pushed_branch,
                    &target_tips,
                )
                .await?
            } else {
                ProposalBaseInference::NotFound
            };
            let inferred_base = match &inference {
                ProposalBaseInference::Selected(base) => Some(base.clone()),
                ProposalBaseInference::NotFound
                | ProposalBaseInference::ParentContainedByTarget => None,
            };
            let selected_base = explicit_base.clone().or(inferred_base);
            let preserved_base = if selected_base.is_none()
                && matches!(inference, ProposalBaseInference::NotFound)
                && !refspec.starts_with('+')
            {
                merge_base_for_fast_forward_update(
                    git_repo,
                    patches
                        .first()
                        .context("existing proposal has no tip event")?,
                    &tip_of_pushed_branch,
                )?
            } else {
                None
            };
            if let Some(base) = &selected_base {
                commits_after_base(git_repo, base, &tip_of_pushed_branch)?;
            }
            let proposal_metadata = ngit::push::ProposalMetadata {
                target_branch: inherited_target,
                explicit_base: selected_base
                    .as_ref()
                    .map(|base| base.commit)
                    .or(preserved_base),
            };
            if proposal.pubkey == user_ref.public_key
                || repo_ref.is_authorized_maintainer(&user_ref.public_key)
            {
                if refspec.starts_with('+') {
                    // force push
                    let (ahead, default_label) = if let Some(base) = &selected_base {
                        (
                            commits_after_base(git_repo, base, &tip_of_pushed_branch)?,
                            base.description.clone(),
                        )
                    } else if let Some(branch) = &proposal_metadata.target_branch {
                        (
                            git_repo.get_commits_ahead_of_branch(&tip_of_pushed_branch, branch)?,
                            format!("target branch '{branch}'"),
                        )
                    } else {
                        git_repo
                            .get_commits_ahead_of_default(&tip_of_pushed_branch, default_branch)?
                    };
                    if ahead.is_empty() {
                        bail!(
                            "cannot push '{from}' as proposal as branch isn't ahead of {default_label}"
                        );
                    }
                    for patch in generate_patches_or_pr_event_or_pr_updates(
                        client,
                        git_repo,
                        repo_ref,
                        &ahead,
                        user_ref,
                        Some(effective_root),
                        signer,
                        term,
                        title_description,
                        git_server_push_options,
                        git_server,
                        default_branch,
                        &proposal_metadata,
                        patches.first(),
                    )
                    .await?
                    {
                        events.push(patch);
                    }
                } else {
                    // fast forward push
                    let tip_patch = patches.first().unwrap();
                    let tip_of_proposal = get_commit_id_from_patch(tip_patch)?;
                    let tip_of_proposal_commit =
                        git_repo.get_commit_or_tip_of_reference(&tip_of_proposal)?;

                    let (mut ahead, behind) = git_repo
                        .get_commits_ahead_behind(&tip_of_proposal_commit, &tip_of_pushed_branch)?;
                    if behind.is_empty() {
                        let thread_id = if let Ok(root_event_id) = get_event_root(tip_patch) {
                            root_event_id
                        } else {
                            // tip patch is the root proposal
                            tip_patch.id
                        };
                        let mut parent_patch = tip_patch.clone();
                        ahead.reverse();
                        if ahead.is_empty() {
                            bail!(
                                "cannot push '{from}' as proposal as branch isn't ahead of proposal on nostr"
                            );
                        }
                        if effective_root.kind.eq(&KIND_PULL_REQUEST)
                            || git_repo.are_commits_too_big_for_patches(&ahead)
                            || git_repo.do_commits_contain_submodules(&ahead)
                        {
                            for event in generate_patches_or_pr_event_or_pr_updates(
                                client,
                                git_repo,
                                repo_ref,
                                &ahead,
                                user_ref,
                                Some(effective_root),
                                signer,
                                term,
                                title_description,
                                git_server_push_options,
                                git_server,
                                default_branch,
                                &proposal_metadata,
                                patches.first(),
                            )
                            .await?
                            {
                                events.push(event);
                            }
                        } else {
                            for (i, commit) in ahead.iter().enumerate() {
                                let new_patch = generate_patch_event(
                                    git_repo,
                                    &git_repo.get_root_commit()?,
                                    commit,
                                    Some(thread_id),
                                    signer,
                                    repo_ref,
                                    Some(parent_patch.id),
                                    Some((
                                        (patches.len() + i + 1).try_into().unwrap(),
                                        (patches.len() + ahead.len()).try_into().unwrap(),
                                    )),
                                    None,
                                    &None,
                                    &[],
                                )
                                .await
                                .context("failed to make patch event from commit")?;
                                events.push(new_patch.clone());
                                parent_patch = new_patch;
                            }
                        }
                    } else {
                        // we shouldn't get here
                        term.write_line(
                                format!(
                                    "WARNING: failed to push {from} as nostr proposal. Try and force push ",
                                )
                                .as_str(),
                            )
                            .unwrap();
                        println!(
                            "error {to} failed to fastforward as newer patches found on proposal"
                        );
                        rejected_proposal_refspecs.push(refspec.clone());
                    }
                }
            } else {
                println!(
                    "error {to} permission denied. you are not the proposal author or a repo maintainer"
                );
                rejected_proposal_refspecs.push(refspec.clone());
            }
        } else {
            // TODO new proposal / couldn't find exisiting proposal
            let target_tips = if let Some(branch) = &proposal_options.target_branch {
                vec![resolve_target_branch_tip(
                    git_repo,
                    branch,
                    default_branch,
                    true,
                )?]
            } else {
                git_repo.get_default_branch_tips(default_branch)?
            };
            let inferred_base = if explicit_base.is_none() {
                match infer_proposal_base(
                    git_repo,
                    repo_ref,
                    &open_proposals,
                    None,
                    None,
                    current_user,
                    &tip_of_pushed_branch,
                    &target_tips,
                )
                .await?
                {
                    ProposalBaseInference::Selected(base) => Some(base),
                    ProposalBaseInference::NotFound
                    | ProposalBaseInference::ParentContainedByTarget => None,
                }
            } else {
                None
            };
            let selected_base = explicit_base.clone().or(inferred_base);
            let proposal_metadata = ngit::push::ProposalMetadata {
                target_branch: proposal_options.target_branch.clone(),
                explicit_base: selected_base.as_ref().map(|base| base.commit),
            };
            let (ahead, default_label) = if let Some(base) = &selected_base {
                (
                    commits_after_base(git_repo, base, &tip_of_pushed_branch)?,
                    base.description.clone(),
                )
            } else if let Some(branch) = &proposal_options.target_branch {
                (
                    git_repo.get_commits_ahead_of_branch(&tip_of_pushed_branch, branch)?,
                    format!("target branch '{branch}'"),
                )
            } else {
                git_repo.get_commits_ahead_of_default(&tip_of_pushed_branch, default_branch)?
            };
            if ahead.is_empty() {
                bail!("cannot push '{from}' as proposal as branch isn't ahead of {default_label}");
            }
            for event in generate_patches_or_pr_event_or_pr_updates(
                client,
                git_repo,
                repo_ref,
                &ahead,
                user_ref,
                None,
                signer,
                term,
                title_description,
                git_server_push_options,
                git_server,
                default_branch,
                &proposal_metadata,
                None,
            )
            .await?
            {
                events.push(event);
            }
        }
    }

    Ok((events, rejected_proposal_refspecs))
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn generate_patches_or_pr_event_or_pr_updates(
    client: &Client,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    ahead: &[Sha1Hash],
    user_ref: &mut UserRef,
    root_proposal: Option<&Event>,
    signer: &Arc<NgitSigner>,
    term: &Term,
    title_description: Option<&(String, String)>,
    git_server_push_options: &[String],
    git_server: Option<&str>,
    default_branch: Option<&str>,
    proposal_metadata: &ngit::push::ProposalMetadata,
    ordering_reference: Option<&Event>,
) -> Result<Vec<Event>> {
    let parent_is_pr = root_proposal.is_some_and(|proposal| proposal.kind.eq(&KIND_PULL_REQUEST));
    let commits_too_big = git_repo.are_commits_too_big_for_patches(ahead);
    let has_submodules = git_repo.do_commits_contain_submodules(ahead);
    let repo_has_grasp_server = !repo_ref.grasp_servers().is_empty();
    let use_pr = parent_is_pr
        || commits_too_big
        || has_submodules
        || proposal_metadata.target_branch.is_some()
        || proposal_metadata.explicit_base.is_some()
        || (root_proposal.is_none() && repo_has_grasp_server);

    if use_pr {
        let tip = ahead.last().context("no commits")?; // ahead is oldest first (callers reverse it)
        let first_commit = ahead.first().context("no commits")?;
        let push_options_refs: Vec<&str> =
            git_server_push_options.iter().map(String::as_str).collect();
        // Compute the merge-base (fork point) from the actual git topology: the
        // point where this branch diverges from the default branch. Crucially
        // we compare against the *most advanced* default branch visible — the
        // local default branch and every remote's default branch — not just
        // `origin` (the nostr remote), whose view of the default branch can be
        // stale in a multi-remote workflow (e.g. the canonical default branch
        // lives on a gitlab/github remote and the nostr remote lags). Using the
        // stale origin tip produced a fork point that didn't reflect that the
        // default branch had advanced.
        //
        // This is correct for all push types:
        //   - new PR: merge-base(tip, default) == first commit's parent
        //   - FF push on existing PR: merge-base(tip, default) == original fork point
        //     (not the previous PR tip, which parent(ahead.first()) would give for the
        //     truncated FF `ahead` set — the 840c581 bug)
        //   - force push after rebase: merge-base(tip, default) == new fork point (not
        //     the stale value from the original PR event tag)
        // Using the git DAG directly means no stored event values can ever
        // propagate a stale or incorrect fork point.
        let merge_base: Option<Sha1Hash> = if let Some(base) = &proposal_metadata.explicit_base {
            Some(*base)
        } else if let Some(branch) = &proposal_metadata.target_branch {
            Some(
                git_repo
                    .get_most_advanced_merge_base_with_branch(tip, branch)?
                    .with_context(|| {
                        format!("proposal has no common history with target branch '{branch}'")
                    })?,
            )
        } else {
            git_repo
                .get_most_advanced_merge_base_with_default(tip, default_branch)
                .ok()
                .flatten()
        };
        select_servers_push_refs_and_generate_pr_or_pr_update_event(
            client,
            git_repo,
            repo_ref,
            tip,
            first_commit,
            merge_base.as_ref(),
            proposal_metadata,
            user_ref,
            root_proposal,
            &title_description.map(|(t, d)| (t.clone(), d.clone())),
            signer,
            term,
            &push_options_refs,
            git_server,
        )
        .await
        .context(format!(
            "{} run `ngit send` for more options.",
            if parent_is_pr {
                "couldn't generate PR update event."
            } else if commits_too_big || has_submodules {
                "a commit in your proposal is too big for a nostr patch so we tried to create it as a nostr PR instead. Unfortunately this failed."
            } else if proposal_metadata.target_branch.is_some() {
                "the proposal targets a non-default branch so it must be submitted as a PR kind, but creating the PR failed."
            } else if proposal_metadata.explicit_base.is_some() {
                "the proposal selects an explicit base so it must be submitted as a PR kind, but creating the PR failed."
            } else {
                "the repository uses a GRASP server so the proposal was submitted as a PR kind, but creating the PR failed."
            },
        ))
    } else {
        generate_cover_letter_and_patch_events(
            title_description.cloned(),
            git_repo,
            ahead,
            signer,
            repo_ref,
            &root_proposal.map(|proposal| proposal.id.to_string()),
            &[],
            ordering_reference,
        )
        .await
    }
}

type HashMapUrlRefspecs = HashMap<String, Vec<String>>;

/// Also used by `ngit init`'s in-process initial-branch push so a fresh
/// init builds exactly the per-server plans a `git push` through the
/// remote helper would have produced.
///
/// One of two per-server plan builders; the other is
/// `sub_commands::sync::build_state_push_plans`. Converging them was
/// attempted and abandoned: this builder is refspec-driven (it filters
/// and adjusts the refspecs the user asked `git push` to perform,
/// judging each against the nostr baseline *and* every server with
/// three-way ancestry checks), can *reject* a refspec — a concept the
/// state-driven builder has no channel for — with the rejection
/// cascading out of every server's plan and per-ref recovery dialogue
/// printed inline, and special-cases annotated-tag object oids and
/// oid-literal refspecs. `build_state_push_plans` instead derives the
/// goal from a desired state map (emitting deletions for refs absent
/// from it) and defers destructive-refspec policy to the transaction's
/// `ServerForcePolicy`. A shared core would need mode switches for the
/// baseline source, the comparison arity, the reject-vs-drop channel,
/// tag handling and the dialogue sink — more machinery than the two
/// focused builders it would replace.
#[allow(clippy::too_many_lines)]
pub(crate) fn create_rejected_refspecs_and_remotes_refspecs(
    term: &console::Term,
    git_repo: &Repo,
    refspecs: &Vec<String>,
    nostr_state: &HashMap<String, String>,
    list_outputs: &HashMap<String, (HashMap<String, String>, bool)>,
) -> Result<(HashMapUrlRefspecs, HashMapUrlRefspecs)> {
    let mut refspecs_for_remotes = HashMap::new();

    let mut rejected_refspecs: HashMapUrlRefspecs = HashMap::new();

    for (url, (remote_state, is_grasp_server)) in list_outputs {
        let is_grasp_server = is_grasp_server.to_owned();
        let short_name = get_short_git_server_name(url);
        let mut refspecs_for_remote = vec![];
        for refspec in refspecs {
            let (from, to) = refspec_to_from_to(refspec)?;
            let nostr_value = nostr_state.get(to);
            let remote_value = remote_state.get(to);
            if from.is_empty() {
                if remote_value.is_some() {
                    // delete remote branch
                    refspecs_for_remote.push(refspec.clone());
                }
                continue;
            }
            // Handle annotated tags. The source side of a push refspec may be
            // an object ID (or another revision expression), not only a ref.
            if let Some(annotated_tag) = resolve_annotated_tag_source(&git_repo.git_repo, from) {
                if let Some(remote_value) = remote_value {
                    if annotated_tag.id().to_string() == *remote_value {
                        // remote already at correct state
                    } else if is_grasp_server {
                        refspecs_for_remote.push(ensure_force_push_refspec(refspec));
                    } else if refspec.starts_with('+') || refspec.starts_with(':') {
                        refspecs_for_remote.push(refspec.clone());
                    } else {
                        // reject
                        rejected_refspecs
                            .entry(refspec.clone())
                            .and_modify(|a| a.push(url.clone()))
                            .or_insert(vec![url.clone()]);
                        // TODO should we reject or or just warn?
                        term.write_line(
                            format!(
                                "ERROR: {short_name} {to} exists with a different reference. someone else may have pushed new updates. options:\r\n  1. review and integrate remote's tip available via `git checkout {remote_value}` \r\n  2. align remote state with nostr via `ngit sync --ref-name {to} --force` and try to push again",
                            ).as_str(),
                        )?;
                    }
                } else {
                    // push new tag
                    refspecs_for_remote.push(refspec.clone());
                }
                continue;
            }

            let from_tip = git_repo.get_commit_or_tip_of_reference(from)?;
            if let Some(nostr_value) = nostr_value {
                if let Some(remote_value) = remote_value {
                    if nostr_value.eq(remote_value) {
                        // in sync - existing branch at same state
                        let is_remote_tip_ancestor_of_commit = if let Ok(remote_value_tip) =
                            git_repo.get_commit_or_tip_of_reference(remote_value)
                        {
                            if let Ok((_, behind)) =
                                git_repo.get_commits_ahead_behind(&remote_value_tip, &from_tip)
                            {
                                behind.is_empty()
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if is_remote_tip_ancestor_of_commit {
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            refspecs_for_remote.push(ensure_force_push_refspec(refspec));
                        }
                    } else if let Ok(remote_value_tip) =
                        git_repo.get_commit_or_tip_of_reference(remote_value)
                    {
                        if from_tip.eq(&remote_value_tip) {
                            // remote already at correct state
                            write_progress_line(
                                term,
                                format!("{short_name} {to} already up-to-date").as_str(),
                            )?;
                        }
                        let (ahead_of_local, behind_local) =
                            git_repo.get_commits_ahead_behind(&from_tip, &remote_value_tip)?;
                        if ahead_of_local.is_empty() {
                            // can soft push
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            // cant soft push
                            let (ahead_of_nostr, behind_nostr) = git_repo
                                .get_commits_ahead_behind(
                                    &git_repo.get_commit_or_tip_of_reference(nostr_value)?,
                                    &remote_value_tip,
                                )?;
                            if ahead_of_nostr.is_empty() {
                                // ancestor of nostr and we are force pushing anyway...
                                refspecs_for_remote.push(refspec.clone());
                            } else if is_grasp_server {
                                // a grasp server can only be pushed to via nostr so can force push
                                refspecs_for_remote.push(ensure_force_push_refspec(refspec));
                            } else {
                                rejected_refspecs
                                    .entry(refspec.clone())
                                    .and_modify(|a| a.push(url.clone()))
                                    .or_insert(vec![url.clone()]);
                                term.write_line(
                                    format!(
                                        "ERROR: {short_name} {to} conflicts with nostr ({} ahead {} behind) and local ({} ahead {} behind). someone else may have pushed new updates. options:\r\n  1. review and integrate remote's tip available via `git checkout {remote_value}` \r\n  2. align remote state with nostr via `ngit sync --ref-name {to} --force` and try to push again",
                                        ahead_of_nostr.len(),
                                        behind_nostr.len(),
                                        ahead_of_local.len(),
                                        behind_local.len(),
                                    ).as_str(),
                                )?;
                            }
                        }
                    } else if is_grasp_server {
                        refspecs_for_remote.push(ensure_force_push_refspec(refspec));
                    } else {
                        // remote_value oid is not present locally
                        // TODO can we download the remote reference?

                        // cant soft push
                        rejected_refspecs
                            .entry(refspec.clone())
                            .and_modify(|a| a.push(url.clone()))
                            .or_insert(vec![url.clone()]);
                        term.write_line(
                            format!("ERROR: {short_name} {to} conflicts with nostr and is not an ancestor of local branch. someone else may have pushed new updates. options:\r\n  1. review and integrate remote's tip available via `git checkout {remote_value}` \r\n  2. align remote state with nostr via `ngit sync --ref-name {to} --force` and try to push again").as_str(),
                        )?;
                    }
                } else {
                    // existing nostr branch not on remote
                    // report - creating new branch
                    write_progress_line(
                        term,
                        format!(
                            "{short_name} {to} doesn't exist and will be added as a new branch"
                        )
                        .as_str(),
                    )?;
                    refspecs_for_remote.push(refspec.clone());
                }
            } else if let Some(remote_value) = remote_value {
                // new to nostr but on remote
                if let Ok(remote_value_tip) = git_repo.get_commit_or_tip_of_reference(remote_value)
                {
                    let (ahead, behind) =
                        git_repo.get_commits_ahead_behind(&from_tip, &remote_value_tip)?;
                    if ahead.is_empty() {
                        // can soft push
                        refspecs_for_remote.push(refspec.clone());
                    } else if is_grasp_server {
                        refspecs_for_remote.push(ensure_force_push_refspec(refspec));
                    } else {
                        // cant soft push
                        rejected_refspecs
                            .entry(refspec.clone())
                            .and_modify(|a| a.push(url.clone()))
                            .or_insert(vec![url.clone()]);
                        term.write_line(
                                    format!(
                                        "ERROR: {short_name} already contains {to} {} ahead and {} behind local branch. someone else may have pushed new updates. options:\r\n  1. review and integrate remote's tip available via `git checkout {remote_value}` \r\n  2. align remote state with nostr via `ngit sync --ref-name {to} --force` and try to push again",
                                        ahead.len(),
                                        behind.len(),
                                    ).as_str(),
                                )?;
                    }
                } else if is_grasp_server {
                    refspecs_for_remote.push(ensure_force_push_refspec(refspec));
                } else {
                    // havn't fetched oid from remote
                    // TODO fetch oid from remote
                    // cant soft push
                    rejected_refspecs
                        .entry(refspec.clone())
                        .and_modify(|a| a.push(url.clone()))
                        .or_insert(vec![url.clone()]);
                    term.write_line(
                        format!("ERROR: {short_name} already contains {to} at {remote_value} which is not an ancestor of local branch. someone else may have pushed new updates. options:\r\n  1. review and integrate remote's tip available via `git checkout {remote_value}` \r\n  2. align remote state with nostr via `ngit sync --ref-name {to} --force` and try to push again").as_str(),
                    )?;
                }
            } else {
                // in sync - new branch
                refspecs_for_remote.push(refspec.clone());
            }
        }
        // An empty plan is kept: it means every requested change is
        // already applied on this server (e.g. deleting an already-absent
        // branch), so the server counts as a successful push target
        // without anything being pushed.
        refspecs_for_remotes.insert(url.clone(), refspecs_for_remote);
    }

    // remove rejected refspecs so they dont get pushed to some remotes
    let mut remotes_refspecs_without_rejected = HashMap::new();
    for (url, value) in &refspecs_for_remotes {
        remotes_refspecs_without_rejected.insert(
            url.clone(),
            value
                .iter()
                .filter(|refspec| !rejected_refspecs.contains_key(*refspec))
                .cloned()
                .collect(),
        );
    }
    Ok((rejected_refspecs, remotes_refspecs_without_rejected))
}

fn ensure_force_push_refspec(refspec: &str) -> String {
    // Check if the refspec starts with '+' or ':'
    if refspec.starts_with('+') || refspec.starts_with(':') {
        refspec.to_string() // Return as is
    } else {
        format!("+{refspec}") // Add '+' prefix
    }
}

/// Resolve an annotated-tag source without assuming the source is a ref name.
///
/// Git permits any revision expression on the source side of a push refspec,
/// including a raw object ID. A commit resolves successfully but cannot peel
/// to a tag, so it naturally returns `None` and follows the normal commit path.
fn resolve_annotated_tag_source<'repo>(
    git_repo: &'repo Repository,
    source: &str,
) -> Option<git2::Object<'repo>> {
    git_repo
        .revparse_single(source)
        .ok()?
        .peel(git2::ObjectType::Tag)
        .ok()
}

/// Also used by `ngit init`'s in-process initial-branch push (see
/// [`create_rejected_refspecs_and_remotes_refspecs`]).
pub(crate) fn generate_updated_state(
    git_repo: &Repo,
    existing_state: &HashMap<String, String>,
    refspecs: &Vec<String>,
) -> Result<HashMap<String, String>> {
    let mut new_state = existing_state.clone();

    // Backfill missing ^{} peeled refs for any annotated tags already in the
    // state.  State events published before this fix only stored the tag object
    // OID; without the corresponding ^{} entry git cannot resolve the tag to a
    // commit and treats it as missing (git fetch --prune deletes it).  We fix
    // this opportunistically on every push so affected repos self-heal without
    // requiring manual intervention.
    let tag_refs: Vec<(String, String)> = new_state
        .iter()
        .filter(|(k, _)| k.starts_with("refs/tags/") && !k.ends_with("^{}"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (ref_name, tag_oid) in tag_refs {
        let peeled_key = format!("{ref_name}^{{}}");
        if new_state.contains_key(&peeled_key) {
            continue;
        }
        // check if the stored OID is a tag object (annotated tag)
        if let Ok(oid) = git2::Oid::from_str(&tag_oid) {
            if git_repo
                .git_repo
                .find_object(oid, Some(git2::ObjectType::Tag))
                .is_ok()
            {
                // peel to the commit the annotated tag points to
                if let Ok(commit_oid) = git_repo.get_commit_or_tip_of_reference(&ref_name) {
                    new_state.insert(peeled_key, commit_oid.to_string());
                }
            }
        }
    }

    for refspec in refspecs {
        let (from, to) = refspec_to_from_to(refspec)?;
        if from.is_empty() {
            // delete
            new_state.remove(to);
            if to.contains("refs/tags") {
                new_state.remove(&format!("{to}{}", "^{}"));
            }
        } else if to.contains("refs/tags") {
            if let Some(annotated_tag) = resolve_annotated_tag_source(&git_repo.git_repo, from) {
                // this is an annotated tag so there is a tag oid
                // ref points to tag oid
                new_state.insert(to.to_string(), annotated_tag.id().to_string());
                // dereferenced tags ref points to commit at its head
                new_state.insert(
                    format!("{to}{}", "^{}"),
                    annotated_tag
                        .peel_to_commit()
                        .context(format!(
                            "cannot find commit from annotated tag source {from} to push to {to}"
                        ))?
                        .id()
                        .to_string(),
                );
            } else {
                // this is a lightweight tag so there is no tag oid
                new_state.insert(
                    to.to_string(),
                    git_repo
                        .get_commit_or_tip_of_reference(from)
                        .context(format!(
                            "cannot find commit from annotated tag ref {from} to push to {to}"
                        ))?
                        .to_string(),
                );
            }
        } else {
            // add or update
            new_state.insert(
                to.to_string(),
                git_repo
                    .get_commit_or_tip_of_reference(from)
                    .context(format!(
                        "cannot find commit from ref {from} to push to {to}"
                    ))?
                    .to_string(),
            );
        }
    }
    Ok(new_state)
}

struct MergeStatusContext<'a> {
    decoded_nostr_url: &'a NostrUrlDecoded,
    repo_ref: &'a RepoRef,
    git_repo: &'a Repo,
    signer: &'a Arc<NgitSigner>,
    existing_state: &'a HashMap<String, String>,
    declared_default_branch: Option<&'a str>,
}

async fn get_merged_status_events(
    term: &console::Term,
    refspecs_to_git_server: &[String],
    context: MergeStatusContext<'_>,
) -> Result<Vec<Event>> {
    let MergeStatusContext {
        decoded_nostr_url,
        repo_ref,
        git_repo,
        signer,
        existing_state,
        declared_default_branch,
    } = context;

    let mut events = vec![];
    let mut status_events = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![nostr::prelude::Filter::default().kinds(status_kinds().clone())],
    )
    .await?;
    status_events.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    let pr_roots = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![nostr::prelude::Filter::default().kind(KIND_PULL_REQUEST)],
    )
    .await?;

    for refspec in refspecs_to_git_server {
        let (from, to) = refspec_to_from_to(refspec)?;
        {
            let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;
            let tip_of_remote_branch = if let Some(oid) = existing_state.get(to) {
                Sha1Hash::from_str(oid)
                    .with_context(|| format!("state for {to} contains invalid oid {oid}"))?
            } else {
                let Ok(tip_of_remote_branch) =
                    git_repo.get_commit_or_tip_of_reference(&refspec_remote_ref_name(
                        &git_repo.git_repo,
                        refspec,
                        None,
                        &decoded_nostr_url.original_string,
                    )?)
                else {
                    // branch not on remote
                    continue;
                };
                tip_of_remote_branch
            };
            let (ahead, _) =
                git_repo.get_commits_ahead_behind(&tip_of_remote_branch, &tip_of_pushed_branch)?;

            let commit_events = get_events_from_local_cache(
                git_repo.get_path()?,
                vec![
                    nostr::prelude::Filter::default().kind(nostr::prelude::Kind::GitPatch),
                    nostr::prelude::Filter::default().kind(KIND_PULL_REQUEST),
                    nostr::prelude::Filter::default().kind(KIND_PULL_REQUEST_UPDATE),
                    // TODO: limit by repo_ref
                ],
            )
            .await?;

            let mut merged_proposals_info =
                get_merged_proposals_info(git_repo, &ahead, &commit_events).await?;
            merged_proposals_info.retain(|proposal_id, _| {
                let explicit_target = pr_roots
                    .iter()
                    .find(|event| event.id == *proposal_id)
                    .and_then(|event| tag_value(event, "b").ok());
                explicit_target.map_or_else(
                    || is_default_branch_ref(to, declared_default_branch),
                    |branch| to == format!("refs/heads/{branch}"),
                )
            });

            for event in create_merge_events(
                term,
                git_repo,
                repo_ref,
                signer,
                &merged_proposals_info,
                &status_events,
                &pr_roots,
            )
            .await?
            {
                events.push(event);
            }
        }
    }
    Ok(events)
}

fn is_default_branch_ref(to_ref: &str, declared_default_branch: Option<&str>) -> bool {
    if let Some(default_branch) = declared_default_branch {
        return to_ref == format!("refs/heads/{default_branch}");
    }
    to_ref.eq("refs/heads/main") || to_ref.eq("refs/heads/master")
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum IssueReferenceToken {
    Full(EventId),
    Shorthand8(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IssueResolutionMention {
    verb: String,
    phrase: String,
    references: Vec<IssueReferenceToken>,
}

#[allow(clippy::too_many_lines)]
async fn get_issue_resolution_status_events(
    term: &console::Term,
    decoded_nostr_url: &NostrUrlDecoded,
    repo_ref: &RepoRef,
    git_repo: &Repo,
    signer: &Arc<NgitSigner>,
    refspecs_to_git_server: &Vec<String>,
    declared_default_branch: Option<&str>,
) -> Result<Vec<Event>> {
    let issues = get_issues_from_cache(git_repo.get_path()?, repo_ref.coordinates()).await?;
    if issues.is_empty() {
        return Ok(vec![]);
    }

    let issue_status_filters = vec![
        nostr::prelude::Filter::default()
            .kinds(status_kinds().clone())
            .events(issues.iter().map(|e| e.id)),
        nostr::prelude::Filter::default()
            .custom_tags(
                nostr::filter::SingleLetterTag::UPPERCASE_E,
                issues.iter().map(|e| e.id),
            )
            .kinds(status_kinds().clone()),
    ];

    let mut statuses =
        get_events_from_local_cache(git_repo.get_path()?, issue_status_filters).await?;
    statuses.sort_by_key(|e| e.created_at);
    statuses.reverse();

    let signer_pubkey = signer.get_public_key().await?;
    let empty_pr_roots: Vec<Event> = vec![];
    let proposal_events = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![
            nostr::prelude::Filter::default().kind(nostr::prelude::Kind::GitPatch),
            nostr::prelude::Filter::default().kind(KIND_PULL_REQUEST),
            nostr::prelude::Filter::default().kind(KIND_PULL_REQUEST_UPDATE),
        ],
    )
    .await?;
    let mut events = vec![];
    let mut queued_issue_ids: HashSet<EventId> = HashSet::new();

    for refspec in refspecs_to_git_server {
        let (from, to) = refspec_to_from_to(refspec)?;
        if !is_default_branch_ref(to, declared_default_branch) {
            continue;
        }

        let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;
        let Ok(tip_of_remote_branch) =
            git_repo.get_commit_or_tip_of_reference(&refspec_remote_ref_name(
                &git_repo.git_repo,
                refspec,
                None,
                &decoded_nostr_url.original_string,
            )?)
        else {
            // branch not on remote
            continue;
        };

        let (ahead, _) =
            git_repo.get_commits_ahead_behind(&tip_of_remote_branch, &tip_of_pushed_branch)?;
        let merged_proposals_info =
            get_merged_proposals_info(git_repo, &ahead, &proposal_events).await?;

        // Track all merge commits introduced by this push so each referenced
        // source commit can be attributed to the merge commit that actually
        // landed it. `ahead` is youngest-first.
        let merge_commits_in_push = ahead
            .iter()
            .filter_map(|hash| {
                git_repo
                    .git_repo
                    .find_commit(sha1_to_oid(hash).ok()?)
                    .ok()
                    .filter(|c| c.parent_count() > 1)
                    .map(|_| *hash)
            })
            .collect::<Vec<_>>();

        for commit_hash in ahead {
            let commit_message = git_repo
                .get_commit_message(&commit_hash)
                .unwrap_or_default();
            let mentions = extract_issue_resolution_mentions(&commit_message);
            if mentions.is_empty() {
                continue;
            }

            for mention in mentions {
                for reference in &mention.references {
                    let issue = match resolve_issue_reference(reference, &issues) {
                        IssueReferenceResolution::Found(issue) => issue,
                        IssueReferenceResolution::Ambiguous => {
                            term.write_line(
                                format!(
                                    "commit {}: {} reference is ambiguous in this repo, skipping",
                                    short_sha1(&commit_hash),
                                    issue_reference_to_string(reference)
                                )
                                .as_str(),
                            )?;
                            continue;
                        }
                        IssueReferenceResolution::NotFound => continue,
                    };

                    if queued_issue_ids.contains(&issue.id) {
                        continue;
                    }

                    // Match command-level permissions: only the issue author
                    // or a confirmed repository member can change issue
                    // status. Confirmed moderators therefore count here.
                    if issue.pubkey != signer_pubkey
                        && !repo_ref.is_authorized_member(&signer_pubkey)
                    {
                        term.write_line(
                            format!(
                                "commit {} references issue {}, but signer is not authorized to resolve it",
                                short_sha1(&commit_hash),
                                &issue.id.to_hex()[..8]
                            )
                            .as_str(),
                        )?;
                        continue;
                    }

                    let current_status = get_status(&issue, repo_ref, &statuses, &empty_pr_roots);
                    if current_status == Kind::GitStatusApplied
                        || current_status == Kind::GitStatusClosed
                    {
                        continue;
                    }

                    let merge_commit = find_issue_merge_commit_for_source_commit(
                        git_repo,
                        &merge_commits_in_push,
                        commit_hash,
                    )
                    .filter(|merge| *merge != commit_hash);
                    let related_event = if let Some(event_id) = find_issue_resolution_proposal_id(
                        git_repo,
                        &merged_proposals_info,
                        commit_hash,
                        merge_commit,
                    ) {
                        get_event_from_cache_by_id(git_repo, &event_id).await.ok()
                    } else {
                        None
                    };
                    let status_event = create_issue_resolution_status_event(
                        signer,
                        repo_ref,
                        &issue,
                        &mention,
                        commit_hash,
                        merge_commit,
                        related_event.as_ref(),
                        &statuses,
                    )
                    .await?;

                    write_progress_line(
                        term,
                        format!(
                            "commit {}: create issue status resolved event for {}",
                            short_sha1(&commit_hash),
                            &issue.id.to_hex()[..8]
                        )
                        .as_str(),
                    )?;

                    queued_issue_ids.insert(issue.id);
                    statuses.push(status_event.clone());
                    events.push(status_event);
                }
            }
        }
    }

    Ok(events)
}

fn create_issue_resolution_content(
    mention: &IssueResolutionMention,
    source_commit: &Sha1Hash,
    merge_commit: Option<Sha1Hash>,
) -> String {
    let base_phrase = mention.phrase.trim();
    let mut details = if base_phrase.is_empty() {
        mention.verb.clone()
    } else {
        base_phrase.to_string()
    };

    let suffix = if let Some(merge_commit) = merge_commit {
        format!("resolved by commit {source_commit}, when merged in commit {merge_commit}")
    } else {
        format!("resolved by commit {source_commit}")
    };

    if details.is_empty() {
        suffix
    } else {
        details.push_str("\n\n");
        details.push_str(&suffix);
        details
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_issue_resolution_status_event(
    signer: &Arc<NgitSigner>,
    repo_ref: &RepoRef,
    issue: &Event,
    mention: &IssueResolutionMention,
    source_commit: Sha1Hash,
    merge_commit: Option<Sha1Hash>,
    related_event: Option<&Event>,
    statuses: &[Event],
) -> Result<Event> {
    let mut public_keys = repo_ref
        .maintainers
        .iter()
        .copied()
        .collect::<HashSet<PublicKey>>();
    public_keys.insert(issue.pubkey);

    let alt_tag = Tag::parse(["alt", "issue resolved from commit message"])?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let source_commit_tag = Tag::parse(vec!["c".to_string(), source_commit.to_string()])?;
    let merge_commit_tag = merge_commit
        .map(|commit| Tag::parse(vec!["merge-commit".to_string(), commit.to_string()]))
        .transpose()?;
    let related_event_tag = related_event
        .map(|event| {
            Tag::parse(vec![
                "q".to_string(),
                event.id.to_hex(),
                repo_ref
                    .relays
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                event.pubkey.to_hex(),
            ])
        })
        .transpose()?;
    let mut commit_refs = vec![Tag::from(Nip34Tag::Reference(source_commit))];
    if let Some(merge_commit) = merge_commit {
        commit_refs.push(Tag::from(Nip34Tag::Reference(merge_commit)));
    }

    let content = create_issue_resolution_content(mention, &source_commit, merge_commit);

    let statuses: Vec<_> = statuses
        .iter()
        .filter(|event| {
            event.pubkey == issue.pubkey || repo_ref.is_authorized_member(&event.pubkey)
        })
        .cloned()
        .collect();
    sign_ordered_status_event(
        EventBuilder::new(Kind::GitStatusApplied, content).tags(
            [
                vec![
                    alt_tag,
                    Tag::from(Nip10Tag::Event {
                        id: issue.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        marker: Some(Marker::Root),
                        public_key: None,
                    }),
                ],
                [Some(source_commit_tag), merge_commit_tag, related_event_tag]
                    .into_iter()
                    .flatten()
                    .collect(),
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        Tag::from(Nip01Tag::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_hint: c.relays.first().cloned(),
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![r_tag],
                commit_refs,
            ]
            .concat(),
        ),
        signer,
        &statuses,
        issue,
        repo_ref,
        "issue resolved from commit".to_string(),
    )
    .await
}

fn short_sha1(hash: &Sha1Hash) -> String {
    let s = hash.to_string();
    s[..s.len().min(7)].to_string()
}

fn issue_reference_to_string(reference: &IssueReferenceToken) -> String {
    match reference {
        IssueReferenceToken::Full(id) => id.to_hex(),
        IssueReferenceToken::Shorthand8(prefix) => format!("#{prefix}"),
    }
}

enum IssueReferenceResolution {
    Found(Box<Event>),
    NotFound,
    Ambiguous,
}

fn resolve_issue_reference(
    reference: &IssueReferenceToken,
    issues: &[Event],
) -> IssueReferenceResolution {
    match reference {
        IssueReferenceToken::Full(id) => issues
            .iter()
            .find(|e| e.id == *id)
            .cloned()
            .map_or(IssueReferenceResolution::NotFound, |e| {
                IssueReferenceResolution::Found(Box::new(e))
            }),
        IssueReferenceToken::Shorthand8(prefix) => {
            let matches = issues
                .iter()
                .filter(|e| e.id.to_hex().starts_with(prefix))
                .cloned()
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [single] => IssueReferenceResolution::Found(Box::new(single.clone())),
                [] => IssueReferenceResolution::NotFound,
                _ => IssueReferenceResolution::Ambiguous,
            }
        }
    }
}

fn extract_issue_resolution_mentions(commit_message: &str) -> Vec<IssueResolutionMention> {
    let mut mentions = vec![];

    for line in commit_message.lines() {
        let trimmed_line = line.trim();
        if trimmed_line.is_empty() {
            continue;
        }

        let words: Vec<&str> = trimmed_line.split_whitespace().collect();
        for idx in 0..words.len() {
            let Some(verb) = normalized_resolution_verb(words[idx]) else {
                continue;
            };
            let references = parse_issue_reference_tokens(&words[idx + 1..]);
            if references.is_empty() {
                continue;
            }
            mentions.push(IssueResolutionMention {
                verb,
                phrase: trimmed_line.to_string(),
                references,
            });
        }
    }

    mentions
}

fn normalized_resolution_verb(word: &str) -> Option<String> {
    let normalized = word
        .trim_matches(|c: char| {
            c.is_ascii_whitespace()
                || matches!(
                    c,
                    ',' | '.' | ';' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
                )
        })
        .trim_end_matches(':')
        .to_ascii_lowercase();

    if [
        "close",
        "closes",
        "closed",
        "closing",
        "fix",
        "fixes",
        "fixed",
        "fixing",
        "resolve",
        "resolves",
        "resolved",
        "resolving",
        "implement",
        "implements",
        "implemented",
        "implementing",
    ]
    .contains(&normalized.as_str())
    {
        Some(normalized)
    } else {
        None
    }
}

fn parse_issue_reference_tokens(words: &[&str]) -> Vec<IssueReferenceToken> {
    let mut refs = vec![];
    let mut seen_full = HashSet::new();
    let mut seen_short = HashSet::new();

    for raw in words {
        let token = raw.trim_matches(|c: char| {
            c.is_ascii_whitespace()
                || matches!(
                    c,
                    ',' | '.'
                        | ':'
                        | ';'
                        | '!'
                        | '?'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                        | '"'
                        | '\''
                )
        });
        if token.is_empty() {
            continue;
        }

        if let Some(short) = token.strip_prefix('#') {
            if short.len() == 8 && short.chars().all(|c| c.is_ascii_hexdigit()) {
                let lowered = short.to_ascii_lowercase();
                if seen_short.insert(lowered.clone()) {
                    refs.push(IssueReferenceToken::Shorthand8(lowered));
                }
                continue;
            }
        }

        let candidate = token.strip_prefix("nostr:").unwrap_or(token);
        if let Some(event_id) = parse_event_id_token(candidate) {
            if seen_full.insert(event_id) {
                refs.push(IssueReferenceToken::Full(event_id));
            }
        }
    }

    refs
}

fn parse_event_id_token(token: &str) -> Option<EventId> {
    if let Ok(nip19) = Nip19::from_bech32(token) {
        match nip19 {
            Nip19::Event(e) => return Some(e.event_id),
            Nip19::EventId(id) => return Some(id),
            _ => {}
        }
    }

    EventId::from_hex(token).ok()
}

fn find_issue_merge_commit_for_source_commit(
    git_repo: &Repo,
    merge_commits_in_push: &[Sha1Hash],
    source_commit: Sha1Hash,
) -> Option<Sha1Hash> {
    let source_oid = sha1_to_oid(&source_commit).ok()?;

    // `merge_commits_in_push` is in youngest-first order. If multiple merge
    // commits are descendants of `source_commit` in one push, we want the
    // merge that *introduced* it, i.e. the oldest matching descendant in this
    // push batch.
    merge_commits_in_push.iter().copied().rfind(|merge| {
        let Ok(merge_oid) = sha1_to_oid(merge) else {
            return false;
        };
        git_repo
            .git_repo
            .graph_descendant_of(merge_oid, source_oid)
            .unwrap_or(false)
    })
}

/// (`proposal_id`, `revision_id`)
type MergedProposalsInfo =
    HashMap<EventId, (Option<EventId>, HashMap<Sha1Hash, MergedPRCommitType>)>;

/// Find the single proposal that introduced an issue-resolving commit.
///
/// A no-ff merge is matched by its merge commit. For a fast-forwarded
/// proposal, prefer an exact commit match and otherwise use the nearest
/// proposal commit descended from the source commit. Ambiguous matches omit
/// the optional proposal context rather than attaching the wrong event.
fn find_issue_resolution_proposal_id(
    git_repo: &Repo,
    merged_proposals_info: &MergedProposalsInfo,
    source_commit: Sha1Hash,
    merge_commit: Option<Sha1Hash>,
) -> Option<EventId> {
    let context_commit = merge_commit.unwrap_or(source_commit);
    let exact = merged_proposals_info
        .iter()
        .filter(|(_, (_, commits))| commits.contains_key(&context_commit))
        .map(|(proposal_id, _)| *proposal_id)
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return exact.first().copied();
    }
    if !exact.is_empty() || merge_commit.is_some() {
        return None;
    }

    let source_oid = sha1_to_oid(&source_commit).ok()?;
    let mut candidates = merged_proposals_info
        .iter()
        .filter_map(|(proposal_id, (_, commits))| {
            let distance = commits
                .keys()
                .filter_map(|commit| {
                    let commit_oid = sha1_to_oid(commit).ok()?;
                    let (ahead, behind) = git_repo
                        .git_repo
                        .graph_ahead_behind(commit_oid, source_oid)
                        .ok()?;
                    (behind == 0).then_some(ahead)
                })
                .min()?;
            Some((*proposal_id, distance))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(_, distance)| *distance);

    let (proposal_id, distance) = candidates.first().copied()?;
    if candidates.get(1).is_some_and(|(_, next)| *next == distance) {
        None
    } else {
        Some(proposal_id)
    }
}

async fn get_merged_proposals_info(
    git_repo: &Repo,
    ahead: &Vec<Sha1Hash>,
    available_patches_prs_pr_updates: &[Event],
) -> Result<MergedProposalsInfo> {
    let mut proposals: MergedProposalsInfo = HashMap::new();

    for commit_hash in ahead {
        let commit = git_repo.git_repo.find_commit(sha1_to_oid(commit_hash)?)?;
        // three-way merge - just to set merge commit id as the merged branch commits
        // are in ahead
        if commit.parent_count() > 1 {
            for parent in commit.parents() {
                for event in available_patches_prs_pr_updates
                    .iter()
                    .filter(|e| {
                        e.tags.iter().any(|t| {
                            t.as_slice().len() > 1
                                && (t.as_slice()[0].eq("commit") || t.as_slice()[0].eq("c"))
                                && t.as_slice()[1].eq(&parent.id().to_string())
                        })
                    })
                    .collect::<Vec<&Event>>()
                {
                    if let Ok((proposal_id, revision_id)) =
                        get_proposal_and_revision_root_from_patch_or_pr_or_pr_update(
                            git_repo, event,
                        )
                        .await
                    {
                        let (entry_revision_id, merged_patches) =
                            proposals.entry(proposal_id).or_default();
                        if entry_revision_id == &revision_id {
                            merged_patches.insert(*commit_hash, MergedPRCommitType::MergeCommit);
                        }
                    }
                }
            }
        } else {
            // three way merge or fast forward merge commits
            // note: ahead included commits of three-way merged branches
            let mut matching_patches_prs_pr_updates = available_patches_prs_pr_updates
                .iter()
                .filter(|e| {
                    e.tags.iter().any(|t| {
                        t.as_slice().len() > 1
                            && (t.as_slice()[0].eq("commit") || t.as_slice()[0].eq("c"))
                            && t.as_slice()[1].eq(&commit_hash.to_string())
                    })
                })
                .collect::<Vec<&Event>>();
            for patch_event in &matching_patches_prs_pr_updates {
                if let Ok((proposal_id, revision_id)) =
                    get_proposal_and_revision_root_from_patch_or_pr_or_pr_update(
                        git_repo,
                        patch_event,
                    )
                    .await
                {
                    let (entry_revision_id, merged_patches_pr_pr_updates) =
                        proposals.entry(proposal_id).or_default();
                    // ignore revisions without all the merged commits
                    if entry_revision_id == &revision_id {
                        merged_patches_pr_pr_updates.insert(
                            *commit_hash,
                            MergedPRCommitType::PatchCommit {
                                event_id: patch_event.id,
                            },
                        );
                    }
                }
            }
            // applied commits - this is done after so that merged revisions take priority
            if matching_patches_prs_pr_updates.is_empty() {
                let author = git_repo.get_commit_author(commit_hash)?;
                matching_patches_prs_pr_updates = available_patches_prs_pr_updates
                    .iter()
                    .filter(|e| {
                        if let Ok(patch_author) = get_patch_author(e) {
                            patch_author == author
                        } else {
                            false
                        }
                    })
                    .collect::<Vec<&Event>>();
                for patch_event in matching_patches_prs_pr_updates {
                    if let Ok((proposal_id, revision_id)) =
                        get_proposal_and_revision_root_from_patch_or_pr_or_pr_update(
                            git_repo,
                            patch_event,
                        )
                        .await
                    {
                        let (entry_revision_id, merged_patches) =
                            proposals.entry(proposal_id).or_default();
                        // ignore revisions without all the applied commits
                        if entry_revision_id == &revision_id {
                            merged_patches.insert(
                                *commit_hash,
                                MergedPRCommitType::PatchApplied {
                                    event_id: patch_event.id,
                                },
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(proposals)
}

fn get_patch_author(event: &Event) -> Result<Vec<String>> {
    for t in event.tags.clone() {
        match t.as_slice() {
            [tag, name, email, unixtime, offset] if tag == "author" => {
                return Ok(vec![
                    name.clone(),
                    email.clone(),
                    unixtime.clone(),
                    offset.clone(),
                ]);
            }
            _ => (),
        }
    }
    bail!("could not find valid author tag")
}

async fn create_merge_events(
    term: &console::Term,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    signer: &Arc<NgitSigner>,
    merged_proposals_info: &MergedProposalsInfo,
    status_events: &[Event],
    pr_roots: &[Event],
) -> Result<Vec<Event>> {
    let mut events = vec![];
    for (proposal_id, (revision_id, merged_patches)) in merged_proposals_info {
        let proposal = get_event_from_cache_by_id(git_repo, proposal_id).await?;
        if get_status(&proposal, repo_ref, status_events, pr_roots) == Kind::GitStatusApplied {
            continue;
        }

        if merged_patches
            .values()
            .any(|m| *m == MergedPRCommitType::MergeCommit)
        {
            write_progress_line(
                term,
                format!(
                    "merge commit {}: create nostr proposal status event",
                    merged_patches
                        .keys()
                        .next()
                        .map(|h| {
                            let s = h.to_string();
                            s[..s.len().min(7)].to_string()
                        })
                        .unwrap_or_default(),
                )
                .as_str(),
            )?;
        } else if merged_patches
            .values()
            .any(|m| matches!(m, MergedPRCommitType::PatchApplied { .. }))
        {
            write_progress_line(
                term,
                format!(
                    "applied commits from proposal: create nostr proposal status event for {}",
                    event_to_cover_letter(&proposal)?
                        .get_branch_name_with_pr_prefix_and_shorthand_id()?,
                )
                .as_str(),
            )?;
        } else {
            write_progress_line(
                term,
                format!(
                    "fast-forward merge: create nostr proposal status event for {}",
                    event_to_cover_letter(&proposal)?
                        .get_branch_name_with_pr_prefix_and_shorthand_id()?,
                )
                .as_str(),
            )?;
        }
        events.push(
            create_merge_status(
                signer,
                repo_ref,
                &proposal,
                if let Some(revision_id) = revision_id {
                    Some(get_event_from_cache_by_id(git_repo, revision_id).await?)
                } else {
                    None
                }
                .as_ref(),
                if let Some((commit, _)) = merged_patches
                    .iter()
                    .find(|(_, m)| **m == MergedPRCommitType::MergeCommit)
                {
                    vec![*commit]
                } else {
                    // child commits were added to merged_patches first so we reverse it
                    let mut t: Vec<Sha1Hash> = merged_patches.keys().copied().collect();
                    t.reverse();
                    t
                },
                merged_patches
                    .values()
                    .filter_map(|m| match m {
                        MergedPRCommitType::MergeCommit => None,
                        MergedPRCommitType::PatchApplied { event_id }
                        | MergedPRCommitType::PatchCommit { event_id } => Some(*event_id),
                    })
                    .collect(),
                !merged_patches
                    .iter()
                    .any(|(_, m)| *m == MergedPRCommitType::MergeCommit)
                    && merged_patches
                        .values()
                        .any(|m| matches!(m, MergedPRCommitType::PatchApplied { .. })),
                status_events,
            )
            .await?,
        );
    }
    Ok(events)
}

#[derive(PartialEq, Debug)]
enum MergedPRCommitType {
    MergeCommit,
    PatchCommit { event_id: EventId },
    PatchApplied { event_id: EventId },
}

#[allow(clippy::too_many_arguments)]
async fn create_merge_status(
    signer: &Arc<NgitSigner>,
    repo_ref: &RepoRef,
    proposal: &Event,
    revision: Option<&Event>,
    merge_commits: Vec<Sha1Hash>,
    merged_patches: Vec<EventId>,
    applied: bool,
    status_events: &[Event],
) -> Result<Event> {
    let mut public_keys = repo_ref
        .maintainers
        .iter()
        .copied()
        .collect::<HashSet<PublicKey>>();
    public_keys.insert(proposal.pubkey);
    if let Some(revision) = revision {
        public_keys.insert(revision.pubkey);
    }
    let alt_tag = Tag::parse(["alt", "git proposal merged / applied"])?;
    let q_tags = merged_patches
        .iter()
        .map(|merged_patch| Tag::parse(["q", &merged_patch.to_hex()]))
        .collect::<Result<Vec<_>, _>>()?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let kind_str = if applied {
        "applied-as-commits"
    } else {
        "merge-commit-id"
    };
    let commit_strs: Vec<String> = merge_commits.iter().map(ToString::to_string).collect();
    let mut parts: Vec<&str> = vec![kind_str];
    parts.extend(commit_strs.iter().map(String::as_str));
    let kind_tag = Tag::parse(parts)?;
    sign_ordered_status_event(
        EventBuilder::new(nostr::event::Kind::GitStatusApplied, String::new()).tags(
            [
                vec![
                    alt_tag,
                    Tag::from(nostr::nips::nip10::Nip10Tag::Event {
                        id: proposal.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        marker: Some(Marker::Root),
                        public_key: None,
                    }),
                ],
                // Tags for merged patches
                q_tags,
                if let Some(revision) = revision {
                    vec![Tag::from(nostr::nips::nip10::Nip10Tag::Event {
                        id: revision.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        marker: Some(Marker::Root),
                        public_key: None,
                    })]
                } else {
                    vec![]
                },
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        Tag::from(Nip01Tag::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_hint: c.relays.first().cloned(),
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![r_tag, kind_tag],
                merge_commits
                    .iter()
                    .map(|merge_commit| Tag::from(Nip34Tag::Reference(*merge_commit)))
                    .collect::<Vec<Tag>>(),
            ]
            .concat(),
        ),
        signer,
        status_events,
        proposal,
        repo_ref,
        "PR merge".to_string(),
    )
    .await
}

async fn get_proposal_and_revision_root_from_patch_or_pr_or_pr_update(
    git_repo: &Repo,
    event: &Event,
) -> Result<(EventId, Option<EventId>)> {
    if event.kind.eq(&KIND_PULL_REQUEST) {
        return Ok((event.id, None));
    } else if event.kind.eq(&KIND_PULL_REQUEST_UPDATE) {
        if let Some(root) = extract_pr_update_root(event) {
            if let CommentTarget::Event {
                id,
                relay_hint: _,
                pubkey_hint: _,
                kind,
            } = root
            {
                if let Some(kind) = kind {
                    if !kind.eq(&KIND_PULL_REQUEST) {
                        bail!(
                            "pull request update {} root event is {} and not a pull request kind",
                            { event.id.to_bech32()? },
                            kind
                        );
                    }
                }
                return Ok((id, None));
            }
            bail!(
                "pull request update {} root event is not a pull request event",
                event.id.to_bech32()?
            );
        }
        bail!(
            "pull request update {} root event is not a pull request event",
            { event.id.to_bech32()? }
        );
    }

    let proposal_or_revision = get_proposal_or_revision_event(git_repo, event).await?;

    if !proposal_or_revision.kind.eq(&Kind::GitPatch) {
        bail!("thread root is not a git patch");
    }

    if proposal_or_revision.tags.iter().any(|t| {
        t.as_slice().len() > 1
            && ["revision-root", "root-revision"].contains(&t.as_slice()[1].as_str())
    }) {
        Ok((
            EventId::parse(
                &proposal_or_revision
                    .tags
                    .iter()
                    .find(|t| Nip10Tag::parse(t.as_slice()).is_ok_and(|n| n.is_reply()))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "revision-root patch event {} missing reply tag",
                            proposal_or_revision.id
                        )
                    })?
                    .as_slice()[1],
            )?,
            Some(proposal_or_revision.id),
        ))
    } else {
        Ok((proposal_or_revision.id, None))
    }
}

// Temporary patch: nip22::extract_root currently only supports kind-1111
// events, so PR updates need to read their uppercase E root directly until this
// lands:
// nostr:nevent1qy28wumn8ghj7un9d3shjtnwva5hgtnyv4mqqgpvqufjr53e8xrzvsx6whg8aptd3hqfzjllssa33chtu72t9jrgev6cghgp
fn extract_pr_update_root(event: &Event) -> Option<CommentTarget<'_>> {
    let id = event.tags.iter().find_map(|tag| {
        let tag = tag.as_slice();
        (tag.first().map(String::as_str) == Some("E"))
            .then(|| tag.get(1))
            .flatten()
            .and_then(|id| EventId::parse(id).ok())
    })?;

    let kind = event.tags.iter().find_map(|tag| {
        let tag = tag.as_slice();
        (tag.first().map(String::as_str) == Some("K"))
            .then(|| tag.get(1))
            .flatten()
            .and_then(|kind| kind.parse::<u16>().ok())
            .map(Kind::from_u16)
    });

    Some(CommentTarget::Event {
        id,
        relay_hint: None,
        pubkey_hint: None,
        kind,
    })
}

async fn get_proposal_or_revision_event(git_repo: &Repo, event: &Event) -> Result<Event> {
    if event
        .tags
        .iter()
        .any(|t| t.as_slice().len() > 1 && t.as_slice()[1].eq("root"))
    {
        return Ok(event.clone());
    }
    let proposal_or_revision_id = EventId::parse(
        &if let Some(t) = event
            .tags
            .iter()
            .find(|t| Nip10Tag::parse(t.as_slice()).is_ok_and(|n| n.is_root()))
        {
            t.clone()
        } else if let Some(t) = event
            .tags
            .iter()
            .find(|t| Nip10Tag::parse(t.as_slice()).is_ok_and(|n| n.is_reply()))
        {
            t.clone()
        } else {
            Tag::event(event.id)
        }
        .as_slice()[1]
            .clone(),
    )?;
    let cached = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![nostr::prelude::Filter::default().id(proposal_or_revision_id)],
    )
    .await?;
    cached
        .first()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "proposal or revision root event {proposal_or_revision_id} not found in local cache",
            )
        })
        .cloned()
}

/// Delete the legacy tracking ref an old ngit version may have written
/// for a pushed tag. A no-op for non-tag destinations.
///
/// This is the only local ref bookkeeping the helper performs. For
/// every refspec the helper reports `ok`, git's own transport layer
/// updates (or deletes) the remote-tracking ref after the helper
/// exits, mapping the destination through `remote.<name>.fetch`
/// (`transport.c::update_tracking_ref`), so duplicating those writes
/// here could only match git or diverge from it. Tags are the
/// exception git never touches: git does not track tags per-remote —
/// the local `refs/tags/<name>` is the single source of truth and
/// `ngit sync` sources its push refspecs directly from the nostr state
/// event oid (`<oid>:refs/tags/<name>`) — but an earlier version of
/// ngit wrote tag tracking refs to `refs/remotes/<nostr>/<tagname>`,
/// the namespace git uses for remote-tracking branches, making pushed
/// tags appear as remote branches in `git branch -r`, IDEs, etc.
/// Delete any such legacy entry for this tag so the next
/// `git branch -r` is clean.
///
/// Best-effort when the remote name cannot be resolved: a raw-URL push
/// (`git push <nostr-url>` with no configured remote) has no tracking
/// namespace to heal — old ngit versions failed those pushes outright,
/// so no legacy entry can exist.
///
/// Deliberately not converged with
/// `push_bookkeeping::record_accepted_push_refspecs`, which replicates
/// git's bookkeeping where ngit pushes in-process and git never runs.
/// See the `push_bookkeeping` module docs for the boundary rationale.
fn self_heal_legacy_tag_tracking_ref(
    git_repo: &Repository,
    refspec: &str,
    remote_name: Option<&str>,
    nostr_remote_url: &str,
) -> Result<()> {
    let (_, to) = refspec_to_from_to(refspec)?;
    if !to.starts_with("refs/tags/") {
        return Ok(());
    }
    let Ok(legacy_ref_name) =
        refspec_remote_ref_name(git_repo, refspec, remote_name, nostr_remote_url)
    else {
        return Ok(());
    };
    if let Ok(mut legacy_ref) = git_repo.find_reference(&legacy_ref_name) {
        let _ = legacy_ref.delete();
    }
    Ok(())
}

fn refspec_to_from_to(refspec: &str) -> Result<(&str, &str)> {
    if !refspec.contains(':') {
        bail!("refspec should contain a colon (:) but consists of: {refspec}");
    }
    let parts = refspec.split(':').collect::<Vec<&str>>();
    Ok((
        if parts.first().unwrap().starts_with('+') {
            &parts.first().unwrap()[1..]
        } else {
            parts.first().unwrap()
        },
        parts.get(1).unwrap(),
    ))
}

fn refspec_remote_ref_name(
    git_repo: &Repository,
    refspec: &str,
    remote_name: Option<&str>,
    nostr_remote_url: &str,
) -> Result<String> {
    let (_, to) = refspec_to_from_to(refspec)?;
    let remote_name = remote_name
        .map(str::to_string)
        .map_or_else(|| get_remote_name_by_url(git_repo, nostr_remote_url), Ok)?;
    let nostr_remote = git_repo
        .find_remote(&remote_name)
        .context("we should have just located this remote")?;
    let short_name = if let Some(s) = to.strip_prefix("refs/heads/") {
        s.to_string()
    } else if let Some(s) = to.strip_prefix("refs/tags/") {
        s.to_string()
    } else {
        to.to_string()
    };
    Ok(format!(
        "refs/remotes/{}/{}",
        nostr_remote
            .name()
            .ok()
            .flatten()
            .context("remote should have a name")?,
        short_name,
    ))
}

// this maybe a commit id or a ref: pointer
#[allow(dead_code)] // currently unused; kept as-is by the executable consolidation
fn reference_to_ref_value(git_repo: &Repository, reference: &str) -> Result<String> {
    let reference_obj = git_repo
        .find_reference(reference)
        .context(format!("failed to find reference: {reference}"))?;
    if let Ok(Some(symref)) = reference_obj.symbolic_target() {
        Ok(symref.to_string())
    } else {
        Ok(reference_obj
            .peel_to_commit()
            .context(format!("failed to get commit from reference: {reference}"))?
            .id()
            .to_string())
    }
}

fn get_refspecs_from_push_batch(stdin: &Stdin, initial_refspec: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut refspecs = vec![initial_refspec.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["push", spec] => {
                refspecs.push((*spec).to_string());
            }
            [] => break,
            _ => {
                bail!("after a `push` command we are only expecting another push or an empty line")
            }
        }
    }
    Ok(refspecs)
}

#[cfg(test)]
mod tests {
    use nostr::nips::nip19::Nip19Event;

    use super::*;

    mod force_with_lease {
        use super::*;

        const OLD_OID: &str = "0123456789abcdef0123456789abcdef01234567";
        const OTHER_OID: &str = "89abcdef0123456789abcdef0123456789abcdef";

        #[test]
        fn matching_lease_authorizes_force_update() {
            let mut state_refspecs = vec!["refs/heads/main:refs/heads/main".to_string()];
            let mut proposal_refspecs = Vec::new();
            let leases =
                HashMap::from([("refs/heads/main".to_string(), Some(OLD_OID.to_string()))]);
            let advertised = HashMap::from([("refs/heads/main".to_string(), OLD_OID.to_string())]);

            apply_force_with_lease(
                &mut state_refspecs,
                &mut proposal_refspecs,
                &leases,
                &advertised,
            )
            .unwrap();

            assert_eq!(state_refspecs, ["+refs/heads/main:refs/heads/main"]);
        }

        #[test]
        fn stale_lease_rejects_update() {
            let mut state_refspecs = vec!["refs/heads/main:refs/heads/main".to_string()];
            let mut proposal_refspecs = Vec::new();
            let leases =
                HashMap::from([("refs/heads/main".to_string(), Some(OLD_OID.to_string()))]);
            let advertised =
                HashMap::from([("refs/heads/main".to_string(), OTHER_OID.to_string())]);

            apply_force_with_lease(
                &mut state_refspecs,
                &mut proposal_refspecs,
                &leases,
                &advertised,
            )
            .unwrap();

            assert!(state_refspecs.is_empty());
        }
    }

    mod refspec_to_from_to {
        use super::*;

        #[test]
        fn trailing_plus_stripped() {
            let (from, _) = refspec_to_from_to("+testing:testingb").unwrap();
            assert_eq!(from, "testing");
        }
    }

    mod raw_commit_refspecs {
        use super::*;

        fn repo_with_initial_commit() -> Result<(tempfile::TempDir, Repo, git2::Oid)> {
            let dir = tempfile::tempdir()?;
            let git_repo = Repository::init(dir.path())?;
            let commit_oid = {
                let tree_oid = git_repo.index()?.write_tree()?;
                let tree = git_repo.find_tree(tree_oid)?;
                let signature = git2::Signature::now("Test User", "test@example.com")?;
                git_repo.commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    "initial commit",
                    &tree,
                    &[],
                )?
            };

            Ok((dir, Repo { git_repo }, commit_oid))
        }

        #[test]
        fn planner_accepts_raw_commit_oid() -> Result<()> {
            let (_dir, git_repo, commit_oid) = repo_with_initial_commit()?;
            let refspec = format!("{commit_oid}:refs/heads/from-oid");
            let refspecs = vec![refspec.clone()];
            let server_url = "https://example.com/repo.git".to_string();
            let list_outputs = HashMap::from([(
                server_url.clone(),
                (HashMap::<String, String>::new(), false),
            )]);

            let (rejected, plans) = create_rejected_refspecs_and_remotes_refspecs(
                &Term::buffered_stderr(),
                &git_repo,
                &refspecs,
                &HashMap::new(),
                &list_outputs,
            )?;

            assert!(rejected.is_empty());
            assert_eq!(plans.get(&server_url), Some(&vec![refspec]));
            Ok(())
        }

        #[test]
        fn state_generation_accepts_raw_commit_oid_for_branches_and_tags() -> Result<()> {
            let (_dir, git_repo, commit_oid) = repo_with_initial_commit()?;
            let branch = "refs/heads/from-oid";
            let tag = "refs/tags/from-oid";
            let refspecs = vec![
                format!("{commit_oid}:{branch}"),
                format!("{commit_oid}:{tag}"),
            ];

            let state = generate_updated_state(&git_repo, &HashMap::new(), &refspecs)?;

            assert_eq!(state.get(branch), Some(&commit_oid.to_string()));
            assert_eq!(state.get(tag), Some(&commit_oid.to_string()));
            assert!(!state.contains_key(&format!("{tag}^{{}}")));
            Ok(())
        }
    }

    mod issue_resolution_mentions {
        use super::*;

        #[test]
        fn parses_implement_with_shorthand_issue_reference() {
            let mentions =
                extract_issue_resolution_mentions("implement #0afd5344 offline queue syncing");

            assert_eq!(mentions.len(), 1);
            assert_eq!(mentions[0].verb, "implement");
            assert!(
                mentions[0]
                    .references
                    .contains(&IssueReferenceToken::Shorthand8("0afd5344".to_string()))
            );
        }

        #[test]
        fn parses_full_hex_nevent_and_nostr_nevent() {
            let id = EventId::from_hex(
                "0afd5344c3345f0603bc5f5e81c8c62b963cefc1cb178c6ba603bb85a0821ef0",
            )
            .unwrap();
            let nevent = Nip19Event {
                event_id: id,
                relays: vec![],
                author: None,
                kind: None,
            }
            .to_bech32()
            .unwrap();
            let content = format!(
                "fixed <{}> and resolves {} and closes nostr:{}",
                id.to_hex(),
                nevent,
                nevent
            );

            let mentions = extract_issue_resolution_mentions(&content);

            assert!(
                mentions
                    .iter()
                    .any(|m| m.references.contains(&IssueReferenceToken::Full(id)))
            );
        }

        #[test]
        fn resolution_content_includes_phrase_and_merge_context() {
            let mention = IssueResolutionMention {
                verb: "fixes".to_string(),
                phrase: "fixes #0afd5344 offline queue".to_string(),
                references: vec![IssueReferenceToken::Shorthand8("0afd5344".to_string())],
            };
            let source = "1111111111111111111111111111111111111111"
                .parse::<Sha1Hash>()
                .unwrap();
            let merge = "2222222222222222222222222222222222222222"
                .parse::<Sha1Hash>()
                .unwrap();

            let content = create_issue_resolution_content(&mention, &source, Some(merge));

            assert!(content.contains("fixes #0afd5344 offline queue"));
            assert!(content.contains(&format!(
                "resolved by commit {source}, when merged in commit {merge}"
            )));
        }

        #[test]
        fn does_not_parse_shorthand_without_hash_prefix() {
            let mentions =
                extract_issue_resolution_mentions("fixes 0afd5344 offline queue syncing");

            assert!(
                mentions.is_empty(),
                "8-char shorthand without # prefix should not be interpreted as an issue ref"
            );
        }

        #[test]
        fn does_not_parse_invalid_shorthand_lengths_or_chars() {
            let mentions = extract_issue_resolution_mentions(
                "fixes #0afd534 and fixes #0afd5344zz and fixes #0afd53444",
            );

            assert!(
                mentions.is_empty(),
                "invalid shorthand tokens should not be interpreted as issue refs"
            );
        }

        #[test]
        fn does_not_parse_non_event_bech32_as_issue_reference() {
            let mentions = extract_issue_resolution_mentions(
                "fixes nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6",
            );

            assert!(
                mentions.is_empty(),
                "npub/nprofile tokens should not be interpreted as issue refs"
            );
        }

        #[test]
        fn does_not_parse_event_reference_without_resolution_verb() {
            let id = EventId::from_hex(
                "0afd5344c3345f0603bc5f5e81c8c62b963cefc1cb178c6ba603bb85a0821ef0",
            )
            .unwrap();
            let mentions = extract_issue_resolution_mentions(&format!(
                "offline queue touches {} but no status keyword present",
                id.to_hex()
            ));

            assert!(
                mentions.is_empty(),
                "event refs should only trigger when preceded by a recognized resolution verb"
            );
        }
    }
}
