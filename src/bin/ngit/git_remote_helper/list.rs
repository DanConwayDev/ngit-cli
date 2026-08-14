use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use client::get_state_from_cache;
use git::RepoActions;
use ngit::{
    client::{self, FetchReport, is_verbose},
    fetch::fetch_from_git_server,
    git::{self, Repo},
    git_events::{KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, event_to_cover_letter, tag_value},
    list::list_from_remotes,
    login::get_curent_user,
    repo_ref::{self},
    repo_state::RepoState,
    utils::{get_all_proposals, get_open_or_draft_proposals},
};
use repo_ref::RepoRef;

use super::fetch::make_commits_for_proposal;

const AUTO_PR_BRANCHES_CONFIG: &str = "nostr.auto-pr-branches";

#[derive(Clone)]
pub(super) struct ListResult {
    pub(super) remote_states: HashMap<String, (HashMap<String, String>, bool)>,
    pub(super) advertised_refs: HashMap<String, String>,
}

#[allow(clippy::too_many_lines)]
pub async fn run_list(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    for_push: bool,
    fetch_report: &FetchReport,
) -> Result<ListResult> {
    let nostr_state = (get_state_from_cache(Some(git_repo.get_path()?), repo_ref).await).ok();

    let term = console::Term::stderr();

    if is_verbose() {
        term.write_line("git servers: listing refs...")?;
    }
    let nostr_git_url = repo_ref.to_nostr_git_url(&None);
    // nostr_state is passed to list_from_remotes only for the sync-status
    // display; the actual ref state we advertise is determined below.
    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        &nostr_git_url,
        nostr_state.as_ref(),
    )
    .await;

    // Collect all OIDs confirmed present on at least one git server.
    let git_server_oids: std::collections::HashSet<String> = remote_states
        .values()
        .flat_map(|(state, _)| state.values())
        .filter(|v| !v.starts_with("ref: "))
        .cloned()
        .collect();

    // From the per-relay state events captured during the nostr fetch, find
    // the newest state event whose every OID is either:
    //   (a) confirmed present on at least one git server, or
    //   (b) already available locally.
    // This prevents advertising refs whose git objects haven't been pushed to
    // any server yet, which would cause `git clone` / `git fetch` to fail.
    //
    // filter by maintainers to avoid state events from other remotes with the
    // same identifier being selected when they have a newer created_at
    let mut candidates: Vec<&nostr::prelude::Event> = fetch_report
        .state_per_relay
        .values()
        .filter_map(|maybe| maybe.as_ref())
        .filter(|event| repo_ref.maintainers.contains(&event.pubkey))
        .collect();
    // Sort newest-first using NIP-01 replacement ordering: the lower event ID
    // wins when timestamps tie.
    candidates.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    // Deduplicate by event id so we don't check the same event twice.
    candidates.dedup_by_key(|e| e.id);

    let best_state: Option<HashMap<String, String>> = candidates.into_iter().find_map(|event| {
        if let Ok(rs) = RepoState::try_from(vec![event.clone()]) {
            let all_resolvable = rs.state.values().all(|v| {
                v.starts_with("ref: ")
                    || git_server_oids.contains(v)
                    || git_repo.does_commit_exist(v).is_ok_and(|exists| exists)
            });
            if all_resolvable { Some(rs.state) } else { None }
        } else {
            None
        }
    });

    let mut state = if let Some(state) = best_state {
        state
    } else {
        // No relay returned a state event whose OIDs are all resolvable
        // (either no state events were seen on any relay, or every candidate
        // references git objects not yet on any server).  Fall back to
        // whatever the git servers actually report so we never advertise OIDs
        // that cannot be fetched.
        let (state, _is_grasp_server) = repo_ref
            .git_server
            .iter()
            .filter_map(|server| remote_states.get(server))
            .cloned()
            .collect::<Vec<(HashMap<String, String>, bool)>>()
            .first()
            .context("failed to get refs from git server")?
            .clone();
        state
    };

    state.retain(|k, _| !k.starts_with("refs/heads/pr/"));
    let auto_pr_branches = auto_pr_branches_enabled(git_repo)?;

    state.extend(
        // get as refs/heads/pr/<branch-name>(<shorthand-event-id>)
        get_open_and_draft_proposals_state(
            &term,
            git_repo,
            repo_ref,
            &remote_states,
            auto_pr_branches,
        )
        .await?,
    );

    if auto_pr_branches {
        state.extend(
            // get as refs/pr/<branch-name>(<shorthand-event-id>) and refs/pr/<event-id>/head
            get_all_proposals_state(git_repo, repo_ref).await?,
        );
    }

    // TODO 'for push' should we check with the git servers to see if any of them
    // allow push from the user?
    let advertised_refs = state.clone();
    for (name, value) in state {
        if value.starts_with("ref: ") {
            if !for_push {
                println!("{} {name}", value.replace("ref: ", "@"));
            }
        } else {
            println!("{value} {name}");
        }
    }

    println!();
    Ok(ListResult {
        remote_states,
        advertised_refs,
    })
}

/// Advertise open and draft proposals as branches. When automatic PR branches
/// are disabled, only proposals with a matching local branch are included —
/// for a proposal author that is either the bare branch name or the
/// shorthand-id suffixed name that `ngit pr checkout` creates.
#[allow(clippy::too_many_lines)]
async fn get_open_and_draft_proposals_state(
    term: &console::Term,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
    auto_pr_branches: bool,
) -> Result<HashMap<String, String>> {
    let selected_local_branches = if auto_pr_branches {
        None
    } else {
        let branches = git_repo
            .get_local_branch_names()
            .context("failed to list local branches while selecting proposal branches")?
            .into_iter()
            .filter(|name| name.starts_with("pr/"))
            .collect::<HashSet<_>>();

        if branches.is_empty() {
            return Ok(HashMap::new());
        }
        Some(branches)
    };

    let mut open_and_draft_proposals = get_open_or_draft_proposals(git_repo, repo_ref).await?;
    let current_user = get_curent_user(git_repo)?;

    if let Some(selected_local_branches) = &selected_local_branches {
        open_and_draft_proposals.retain(|_, (proposal, _, _)| {
            selected_branch_names(
                proposal,
                current_user.as_ref(),
                Some(selected_local_branches),
            )
            .is_ok_and(|names| !names.is_empty())
        });

        if open_and_draft_proposals.is_empty() {
            return Ok(HashMap::new());
        }
    }

    // we cannot use commit_id in the latest patch in a proposal because:
    // 1) the `commit` tag is optional
    // 2) if the commit tag is wrong, it will cause errors which stop clone from
    //    working

    // without trusting commit_id we must apply each patch which requires the oid of
    // the parent so we much do a fetch

    for (git_server_url, (oids_from_git_servers, is_grasp_server)) in remote_states {
        if fetch_from_git_server(
            git_repo,
            &oids_from_git_servers
                .iter()
                .filter(|(name, value)| {
                    !name.starts_with("refs/heads/pr/") && !value.starts_with("ref: ")
                })
                .map(|(_, value)| value.clone())
                .collect::<Vec<String>>(),
            git_server_url,
            &repo_ref.to_nostr_git_url(&None),
            term,
            *is_grasp_server,
        )
        .is_ok()
        {
            break;
        }
    }

    // Collect PR/PR-update tip OIDs that are still missing after the bulk prefetch.
    // We borrow proposals here so we can move them in the state-building loop
    // below.
    let mut missing_pr_oids: Vec<String> = open_and_draft_proposals
        .values()
        .filter_map(|(_, events, _)| {
            events
                .iter()
                .find(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
                .and_then(|e| tag_value(e, "c").ok())
        })
        .filter(|tip| !git_repo.does_commit_exist(tip).unwrap_or(false))
        .collect();

    // For each repo git server, batch-fetch the PR tip OIDs it carries that are
    // still missing locally. Only OIDs the server has advertised are included in
    // each batch (avoids all-or-nothing batch-poisoning). We mop up across servers
    // until all missing OIDs are satisfied or all servers are exhausted.
    //
    // NOTE: we intentionally restrict mop-up to the repo's declared git servers
    // (remote_states) and do NOT try the git-server URL carried in the PR event's
    // `clone` tag. A PR submitter could include an arbitrary server URL there;
    // fetching from it unconditionally would let a malicious or slow server
    // delay every clone/fetch. If we later want to support PR-supplied servers,
    // it should be opt-in (e.g. an explicit `--include-pr-servers` flag) so
    // users consciously accept the trust/performance trade-off. PRs whose tip
    // OID isn't carried by any repo git server will simply not be advertised as
    // `refs/heads/pr/*` refs; they are still accessible via their patch events.
    if !missing_pr_oids.is_empty() {
        for (server_url, (server_state, is_grasp)) in remote_states {
            let batch: Vec<String> = missing_pr_oids
                .iter()
                .filter(|oid| server_state.values().any(|v| v == *oid))
                .cloned()
                .collect();
            if batch.is_empty() {
                continue;
            }
            let _ = fetch_from_git_server(
                git_repo,
                &batch,
                server_url,
                &repo_ref.to_nostr_git_url(&None),
                term,
                *is_grasp,
            );
            missing_pr_oids.retain(|oid| !git_repo.does_commit_exist(oid).unwrap_or(false));
            if missing_pr_oids.is_empty() {
                break;
            }
        }
    }

    let mut state = HashMap::new();
    for (_, (proposal, events_to_apply, _)) in open_and_draft_proposals {
        let Ok(branch_names) = selected_branch_names(
            &proposal,
            current_user.as_ref(),
            selected_local_branches.as_ref(),
        ) else {
            continue;
        };
        let Some(branch_name) = branch_names.first() else {
            continue;
        };
        // if events_to_apply contains a PR or PR Update event it should be the only
        // event in the Vec
        if let Some(pr_or_pr_update) = events_to_apply
            .iter()
            .find(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
        {
            match tag_value(pr_or_pr_update, "c") {
                Ok(tip) => {
                    // Only advertise once confirmed locally available — this
                    // guarantees the subsequent fetch phase can serve the object.
                    if git_repo.does_commit_exist(&tip).is_ok_and(|r| r) {
                        for name in &branch_names {
                            state.insert(format!("refs/heads/{name}"), tip.clone());
                        }
                    }
                }
                Err(_) => {
                    let _ = term.write_line(
                            format!(
                                "WARNING: failed to fetch branch {branch_name} error: {} event poorly formatted",
                                if pr_or_pr_update.kind.eq(&KIND_PULL_REQUEST) {
                                    "PR"
                                } else {
                                    "PR update"
                                }
                            )
                            .as_str(),
                        );
                }
            }
        } else {
            match make_commits_for_proposal(git_repo, repo_ref, &events_to_apply) {
                Ok(tip) => {
                    for name in &branch_names {
                        state.insert(format!("refs/heads/{name}"), tip.clone());
                    }
                }
                Err(error) => {
                    if let Ok(Some(public_key)) = get_curent_user(git_repo) {
                        if repo_ref.maintainers.contains(&public_key)
                            || events_to_apply.iter().any(|e| e.pubkey.eq(&public_key))
                        {
                            term.write_line(
                                    format!("WARNING (only shown to maintainers or author): failed to fetch branch {branch_name}, error: {error}")
                                        .as_str(),
                                )?;
                        }
                    }
                }
            }
        }
    }
    Ok(state)
}

fn auto_pr_branches_enabled(git_repo: &Repo) -> Result<bool> {
    let config = git_repo
        .git_repo
        .config()
        .context("failed to open git config while reading proposal branch settings")?;

    match config.get_bool(AUTO_PR_BRANCHES_CONFIG) {
        Ok(enabled) => Ok(enabled),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(true),
        Err(error) => Err(error).context(format!(
            "failed to read {AUTO_PR_BRANCHES_CONFIG} as a boolean"
        )),
    }
}

/// Candidate branch names for a proposal, preferred name first. Proposal
/// authors address their own proposal by its bare branch name, but the
/// shorthand-id suffixed form is kept as a fallback because
/// `ngit pr checkout` creates the suffixed name regardless of authorship.
fn proposal_branch_names(
    proposal: &nostr::prelude::Event,
    current_user: Option<&nostr::prelude::PublicKey>,
) -> Result<Vec<String>> {
    let cover_letter = event_to_cover_letter(proposal)?;
    let suffixed = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
    if current_user.is_some_and(|public_key| proposal.pubkey.eq(public_key)) {
        Ok(vec![
            format!("pr/{}", cover_letter.branch_name_without_id_or_prefix),
            suffixed,
        ])
    } else {
        Ok(vec![suffixed])
    }
}

/// The single preferred branch name for a proposal.
fn proposal_branch_name(
    proposal: &nostr::prelude::Event,
    current_user: Option<&nostr::prelude::PublicKey>,
) -> Result<String> {
    let mut names = proposal_branch_names(proposal, current_user)?;
    Ok(names.swap_remove(0))
}

/// Branch names to advertise for a proposal. With automatic PR branches
/// enabled this is the single preferred name. When disabled, it is every
/// candidate name with a matching local branch, so that an author's
/// `ngit pr checkout` opts their own proposal back in even though checkout
/// creates the suffixed branch name.
fn selected_branch_names(
    proposal: &nostr::prelude::Event,
    current_user: Option<&nostr::prelude::PublicKey>,
    selected_local_branches: Option<&HashSet<String>>,
) -> Result<Vec<String>> {
    let mut names = proposal_branch_names(proposal, current_user)?;
    if let Some(selected) = selected_local_branches {
        names.retain(|name| selected.contains(name));
    } else {
        names.truncate(1);
    }
    Ok(names)
}

/// we assume latest default branch oid has been fetched so patch parent commits
/// are present. doesnt report on proposals failed to recreate
async fn get_all_proposals_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<String, String>> {
    let mut state = HashMap::new();
    let all_proposals = get_all_proposals(git_repo, repo_ref).await?;
    let current_user = get_curent_user(git_repo)?;
    for (proposal, events_to_apply, _) in all_proposals.values() {
        if let Ok(branch_name) = proposal_branch_name(proposal, current_user.as_ref()) {
            if let Some(pr_or_pr_update) = events_to_apply
                .iter()
                .find(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
            {
                if let Ok(tip) = tag_value(pr_or_pr_update, "c") {
                    state.insert(format!("refs/{branch_name}"), tip.clone());
                    state.insert(format!("refs/pr/{}/head", proposal.id), tip);
                }
            } else if let Ok(tip) = make_commits_for_proposal(git_repo, repo_ref, events_to_apply) {
                state.insert(format!("refs/{branch_name}"), tip.clone());
                state.insert(format!("refs/pr/{}/head", proposal.id), tip);
            }
        }
    }
    Ok(state)
}
