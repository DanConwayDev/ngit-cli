use std::path::Path;

use anyhow::{Context, Result, bail};
use bitcoin_hashes::sha1::Hash as Sha1Hash;
use console::Style;
use ngit::{
    client::{Params, get_all_proposal_patch_pr_pr_update_events_from_cache, send_events},
    git_events::{
        EventRefType, KIND_PULL_REQUEST, generate_cover_letter_and_patch_events, tag_value,
    },
    proposal_base::{
        ExplicitBase, ProposalBaseInference, commits_after_base, infer_proposal_base,
        merge_base_for_fast_forward_update, resolve_explicit_base, resolve_target_branch_tip,
    },
    push::select_servers_push_refs_and_generate_pr_or_pr_update_event,
    utils::{get_all_proposals, get_open_or_draft_proposals, proposal_tip_is_pr_or_pr_update},
};
use nostr::prelude::{ToBech32, event::Event, nip10::Nip10Tag, nip19::Nip19Event};

use crate::{
    cli::{Cli, SignerParams},
    cli_interactor::{
        Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms, PromptMultiChoiceParms,
        cli_error,
    },
    client::{
        Client, Connect, get_events_from_local_cache, get_repo_ref_from_cache,
        warn_if_invited_as_maintainer,
    },
    git::{Repo, RepoActions, identify_ahead_behind},
    git_events::{event_is_patch_set_root, event_tag_from_nip19_or_hex},
    login,
    repo_ref::get_repo_coordinates_for_publishing,
    sub_commands::repository_fetch::fetching_with_account,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[arg(default_value = "")]
    /// commits to send as proposal; like in `git format-patch` eg. HEAD~2
    pub(crate) since_or_range: String,
    #[clap(long, value_parser, num_args = 0.., value_delimiter = ' ')]
    /// references to an existing proposal for which this is a new
    /// version and/or events / npubs to tag as mentions
    pub(crate) in_reply_to: Vec<String>,
    /// don't prompt for a cover letter
    #[arg(long, action)]
    pub(crate) no_cover_letter: bool,
    /// optional cover letter subject/title
    #[clap(long, alias = "title")]
    pub(crate) subject: Option<String>,
    #[clap(long)]
    /// optional cover letter description
    pub(crate) description: Option<String>,
    /// publish as Pull Request even if each commit is < 60kb
    #[arg(long, action)]
    pub(crate) force_pr: bool,
    /// publish as Patches even if they may be > 60kb; cannot be used when the
    /// existing proposal is already a PR kind (downgrades are not possible)
    #[arg(long, action)]
    pub(crate) force_patch: bool,
    #[clap(long = "push-option", short = 'o', value_parser, num_args = 0..)]
    /// git push options to pass to the git server (eg. -o secret-scanning.skip)
    pub(crate) push_options: Vec<String>,
    #[clap(long = "git-server")]
    /// git server URL to use for pushing the PR; accepts either a GRASP server
    /// base URL (eg. relay.ngit.dev) or a full clone URL (eg.
    /// <https://github.com/user/repo.git>)
    pub(crate) git_server: Option<String>,
    /// branch this PR should target instead of the repository default
    #[clap(long)]
    pub(crate) target_branch: Option<String>,
    /// commit, branch, root PR, or PR update to use as the base for this
    /// publication. History through this base is excluded from the proposed
    /// changes. Use a commit ID or a verified ref, e.g. --base github/master
    #[clap(long)]
    pub(crate) base: Option<String>,
}

/// Validates send command arguments for non-interactive mode.
///
/// Returns Ok(()) if:
/// - Interactive mode is enabled (all validation happens interactively)
/// - Updating an existing proposal (`in_reply_to` is non-empty)
/// - Using defaults mode (--defaults will fill in gaps)
/// - Both title and description are provided
///
/// Returns an error if:
/// - Description provided without title
/// - Title provided without description
/// - Missing required arguments in non-interactive mode
fn validate_send_args(cli: &Cli, args: &SubCommandArgs) -> Result<()> {
    // Interactive mode handles all validation interactively
    if cli.interactive {
        return Ok(());
    }

    // Description requires subject
    if args.description.is_some() && args.subject.is_none() {
        let message = "ngit send requires --subject when --description is provided";
        let details = vec![("--subject <T>", "cover letter subject")];
        let suggestions = vec![
            "ngit send HEAD~2 --subject \"My Feature\" --description \"Details\"",
            "ngit send --interactive",
        ];
        return Err(cli_error(message, &details, &suggestions));
    }

    // Subject requires description
    if args.subject.is_some() && args.description.is_none() {
        let message = "ngit send requires --description when --subject is provided";
        let details = vec![("--description <D>", "cover letter description")];
        let suggestions = vec![
            "ngit send HEAD~2 --subject \"My Feature\" --description \"Details\"",
            "ngit send --interactive",
        ];
        return Err(cli_error(message, &details, &suggestions));
    }

    // Updating existing proposal - no additional validation needed
    if !args.in_reply_to.is_empty() {
        return Ok(());
    }

    // Defaults mode will fill in gaps
    if cli.defaults {
        return Ok(());
    }

    // Both subject and description provided - all good
    if args.subject.is_some() && args.description.is_some() {
        return Ok(());
    }

    // --no-cover-letter with a range is valid (patches without cover letter)
    if args.no_cover_letter && !args.since_or_range.is_empty() {
        return Ok(());
    }

    // Missing required arguments for non-interactive mode
    let message = "ngit send requires additional arguments";
    let mut details = vec![];
    if args.since_or_range.is_empty() {
        details.push(("<SINCE_OR_RANGE>", "commits to send (eg. HEAD~2)"));
    }
    details.push(("--subject <T> --description <D>", "cover letter details"));
    details.push(("-d, --defaults", "use sensible defaults"));
    details.push(("--interactive", "prompt for values"));
    let suggestions = vec![
        "ngit send HEAD~2 --subject \"My Feature\" --description \"Details\"",
        "ngit send --defaults",
        "ngit send --interactive",
    ];
    Err(cli_error(message, &details, &suggestions))
}

#[allow(clippy::too_many_lines)]
pub async fn launch(
    cli_args: &Cli,
    args: &SubCommandArgs,
    no_fetch: bool,
    signer: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let (main_branch_name, main_tip) = git_repo
        .get_main_or_master_branch()
        .context("the default branches (main or master) do not exist")?;

    // Validate arguments early, before any network calls
    validate_send_args(cli_args, args)?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let mut repo_coordinates = get_repo_coordinates_for_publishing(&git_repo, &mut client).await?;

    if !no_fetch {
        fetching_with_account(
            &git_repo,
            git_repo_path,
            &mut client,
            &mut repo_coordinates,
            signer,
        )
        .await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    let (root_proposal, mention_tags) =
        get_root_proposal_and_mentions_from_in_reply_to(git_repo.get_path()?, &args.in_reply_to)
            .await?;

    if root_proposal.is_some() && args.target_branch.is_some() {
        bail!("--target-branch can only be set when opening a new PR");
    }
    if args.target_branch.is_some() && args.force_patch {
        bail!("--target-branch cannot be combined with --force-patch");
    }
    if args.target_branch.is_some() && args.no_cover_letter {
        bail!("--target-branch cannot be combined with --no-cover-letter");
    }
    if args.base.is_some() && args.force_patch {
        bail!("--base cannot be combined with --force-patch");
    }
    if args.base.is_some() && args.no_cover_letter {
        bail!("--base cannot be combined with --no-cover-letter");
    }

    let proposals = if root_proposal.is_some() || args.base.is_some() {
        Some(get_all_proposals(&git_repo, &repo_ref).await?)
    } else {
        None
    };

    let user_selected_base = if let Some(reference) = &args.base {
        Some(
            resolve_explicit_base(
                &git_repo,
                &repo_ref,
                reference,
                proposals
                    .as_ref()
                    .context("proposal cache was not loaded")?,
            )
            .await?,
        )
    } else {
        None
    };

    let proposal_entry = root_proposal.as_ref().and_then(|selected_root| {
        proposals.as_ref().and_then(|all| {
            all.iter().find(|(root_id, (_, _, pr_upgrade_root))| {
                **root_id == selected_root.id
                    || pr_upgrade_root
                        .as_ref()
                        .is_some_and(|upgrade| upgrade.id == selected_root.id)
            })
        })
    });
    let canonical_root_id = proposal_entry
        .map(|(root_id, _)| *root_id)
        .or_else(|| root_proposal.as_ref().map(|root| root.id));
    let proposal_details = proposal_entry.map(|(_, details)| details);
    let proposal_author = proposal_details
        .map(|(root, _, _)| root.pubkey)
        .or_else(|| root_proposal.as_ref().map(|root| root.pubkey));
    let effective_root = proposal_details
        .and_then(|(_, _, pr_upgrade_root)| pr_upgrade_root.as_ref())
        .or(root_proposal.as_ref());
    let existing_thread_is_pr = if let Some(root_id) = canonical_root_id {
        proposal_tip_is_pr_or_pr_update(git_repo_path, &repo_ref, &root_id).await?
    } else {
        false
    };

    let target_branch = args
        .target_branch
        .clone()
        .or_else(|| effective_root.and_then(|root| tag_value(root, "b").ok()));
    let target_tip = target_branch
        .as_deref()
        .map(|branch| {
            resolve_target_branch_tip(&git_repo, branch, None, args.target_branch.is_some())
        })
        .transpose()?;

    let head = git_repo.get_head_commit()?;
    let target_tips = if let Some(target_tip) = target_tip {
        vec![target_tip]
    } else {
        git_repo.get_default_branch_tips(None)?
    };

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer.info,
        signer.password,
        Some(&client),
        true,
    )
    .await?;

    // Authorization is a UX guard, not enforcement (signatures can't be
    // forged and the original author's events are immutable regardless).
    // It applies only to *updates* of an existing PR thread — appending
    // commits to someone else's pull request (a KIND_PULL_REQUEST_UPDATE
    // threaded onto their proposal). It deliberately does NOT apply to a
    // new patch *revision*, which is a fresh proposal root anyone may
    // publish under NIP-34 (see tests/send_patch_revision.rs).
    if existing_thread_is_pr {
        if let Some(proposal_author) = proposal_author {
            if proposal_author != user_ref.public_key
                && !repo_ref.is_authorized_maintainer(&user_ref.public_key)
            {
                bail!(
                    "only the proposal author or a repository maintainer can update an existing pull request"
                );
            }
        }
    }

    client.set_signer(signer.clone()).await;

    let inference = if user_selected_base.is_none() {
        infer_proposal_base(
            &git_repo,
            &repo_ref,
            &get_open_or_draft_proposals(&git_repo, &repo_ref).await?,
            canonical_root_id,
            proposal_details.and_then(|(_, events, _)| events.first()),
            proposal_author.unwrap_or(user_ref.public_key),
            &head,
            &target_tips,
        )
        .await?
    } else {
        ProposalBaseInference::NotFound
    };
    let inferred_base = match &inference {
        ProposalBaseInference::Selected(base) => Some(base.clone()),
        ProposalBaseInference::NotFound | ProposalBaseInference::ParentContainedByTarget => None,
    };
    let selected_base = user_selected_base.or(inferred_base);
    let preserved_base =
        if selected_base.is_none() && matches!(inference, ProposalBaseInference::NotFound) {
            proposal_details
                .and_then(|(_, events, _)| events.first())
                .map(|latest| merge_base_for_fast_forward_update(&git_repo, latest, &head))
                .transpose()?
                .flatten()
                .map(|commit| ExplicitBase {
                    commit,
                    description: "the previous PR merge base".to_string(),
                })
        } else {
            None
        };
    let explicit_base = selected_base.or(preserved_base);

    let proposal_metadata = ngit::push::ProposalMetadata {
        target_branch: target_branch.clone(),
        explicit_base: explicit_base.as_ref().map(|base| base.commit),
    };

    let automatic_commits = if let Some(base) = &explicit_base {
        let mut commits = commits_after_base(&git_repo, base, &head)?;
        commits.reverse();
        Some(commits)
    } else if let Some(branch) = &target_branch {
        let mut commits = git_repo.get_commits_ahead_of_branch(&head, branch)?;
        commits.reverse();
        Some(commits)
    } else {
        None
    };

    let (proposal_base_name, proposal_base_tip) = if let Some(base) = &explicit_base {
        (base.description.clone(), base.commit)
    } else if let Some(branch) = &target_branch {
        (
            branch.clone(),
            target_tip.context("target branch has no visible tip")?,
        )
    } else {
        (main_branch_name.to_string(), main_tip)
    };

    if let Some(root_ref) = args.in_reply_to.first() {
        if root_proposal.is_some() {
            println!("creating proposal revision for: {root_ref}");
        }
    }

    let mut commits: Vec<Sha1Hash> = {
        if args.since_or_range.is_empty() {
            if let Some(commits) = &automatic_commits {
                commits.clone()
            } else if cli_args.interactive {
                let branch_name = git_repo.get_checked_out_branch_name()?;
                let proposed_commits = if branch_name.eq(main_branch_name) {
                    vec![main_tip]
                } else {
                    let (_, _, ahead, _) = identify_ahead_behind(&git_repo, &None, &None)?;
                    ahead
                };
                choose_commits(&git_repo, proposed_commits)?
            } else {
                // --defaults was validated above, so we know it's set
                let branch_name = git_repo.get_checked_out_branch_name()?;
                let proposed_commits = if branch_name.eq(main_branch_name) {
                    vec![main_tip]
                } else {
                    let (_, _, ahead, _) = identify_ahead_behind(&git_repo, &None, &None)?;
                    ahead
                };
                if proposed_commits.len() > 10 && !cli_args.force {
                    bail!(
                        "too many commits ({}). choose where your proposed changes start with --base <commit-or-ref> (PRs only), specify a range, or use --force to send all selected commits",
                        proposed_commits.len()
                    );
                }
                proposed_commits
            }
        } else {
            git_repo
                .parse_starting_commits(&args.since_or_range)
                .context("failed to parse specified starting commit or range")?
        }
    };

    // Check for too many commits with explicit range
    if commits.len() > 10 && !cli_args.force && !cli_args.interactive {
        bail!(
            "too many commits ({}). choose where your proposed changes start with --base <commit-or-ref> (PRs only), specify a smaller range, or use --force to send all selected commits",
            commits.len()
        );
    }

    if commits.is_empty() {
        bail!(
            "no commits selected; select a commit range, or send as a PR with --base <commit-or-ref> set to the commit before your proposed changes (omit --force-patch and --no-cover-letter)"
        );
    }
    println!("creating proposal from {} commits:", commits.len());

    let dim = Style::new().color256(247);
    for commit in &commits {
        println!(
            "{} {}",
            dim.apply_to(commit.to_string().chars().take(7).collect::<String>()),
            git_repo.get_commit_message_summary(commit)?
        );
    }

    let (first_commit_ahead, behind) = git_repo
        .get_commits_ahead_behind(&proposal_base_tip, commits.last().context("no commits")?)?;

    check_commits_are_suitable_for_proposal(
        cli_args,
        &first_commit_ahead,
        &commits,
        &behind,
        &proposal_base_name,
        &proposal_base_tip,
        !args.force_patch && !args.no_cover_letter,
    )?;

    let commits_too_big = git_repo.are_commits_too_big_for_patches(&commits);
    let has_submodules = git_repo.do_commits_contain_submodules(&commits);
    let repo_has_grasp_server = !repo_ref.grasp_servers().is_empty();
    let should_be_pr = existing_thread_is_pr
        || commits_too_big
        || has_submodules
        || target_branch.is_some()
        || proposal_metadata.explicit_base.is_some()
        || (root_proposal.is_none() && repo_has_grasp_server);

    let as_pr = if args.force_patch {
        if existing_thread_is_pr {
            bail!(
                "cannot downgrade an existing PR proposal to patches; omit --force-patch to send as a PR update"
            );
        }
        false
    } else if args.force_pr {
        true
    } else if args.no_cover_letter && !existing_thread_is_pr {
        // --no-cover-letter on a new proposal explicitly opts out of the PR
        // path (which requires a cover letter); send as patch events instead
        false
    } else {
        should_be_pr
    };

    let cover_letter_title_description = if cli_args.interactive {
        // Interactive flow: prompt for cover letter confirm, title, description
        let title = if as_pr {
            match &args.subject {
                Some(t) => Some(t.clone()),
                None => {
                    if root_proposal.is_none() {
                        Some(
                            Interactor::default()
                                .input(PromptInputParms::default().with_prompt("subject"))?
                                .clone(),
                        )
                    } else {
                        None
                    }
                }
            }
        } else if args.no_cover_letter {
            None
        } else {
            match &args.subject {
                Some(t) => Some(t.clone()),
                None => {
                    if Interactor::default().confirm(
                        PromptConfirmParms::default()
                            .with_default(false)
                            .with_prompt("include cover letter?"),
                    )? {
                        Some(
                            Interactor::default()
                                .input(PromptInputParms::default().with_prompt("subject"))?
                                .clone(),
                        )
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(title) = title {
            Some((
                title,
                if let Some(t) = &args.description {
                    t.clone()
                } else {
                    Interactor::default()
                        .input(PromptInputParms::default().with_prompt("description"))?
                        .clone()
                },
            ))
        } else {
            None
        }
    } else if as_pr {
        // PR always needs cover letter
        let title = match &args.subject {
            Some(t) => t.clone(),
            None if cli_args.defaults => {
                git_repo.get_commit_message_summary(commits.first().context("no commits")?)?
            }
            None => bail!("PR requires --subject and --description (or use --defaults)"),
        };
        let description = match &args.description {
            Some(d) => d.clone(),
            None if cli_args.defaults => {
                let commit = commits.first().context("no commits")?;
                let full_message = git_repo.get_commit_message(commit)?;
                let summary = git_repo.get_commit_message_summary(commit)?;
                full_message
                    .strip_prefix(&summary)
                    .unwrap_or(&full_message)
                    .trim()
                    .to_string()
            }
            None => bail!("PR requires --subject and --description (or use --defaults)"),
        };
        Some((title, description))
    } else {
        // Patch mode
        match (&args.subject, &args.description) {
            (Some(t), Some(d)) => Some((t.clone(), d.clone())),
            (Some(_), None) => bail!("--subject requires --description"),
            (None, Some(_)) => bail!("--description requires --subject"),
            (None, None) => None, // no cover letter
        }
    };

    // oldest first
    commits.reverse();

    let events = if as_pr {
        let tip = commits.last().context("no commits")?; // commits has been reversed to oldest first
        let first_commit = commits.first().context("no commits")?;
        let merge_base = if let Some(base) = &proposal_metadata.explicit_base {
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
            git_repo.get_commit_parent(first_commit).ok()
        };
        {
            let push_options_refs: Vec<&str> =
                args.push_options.iter().map(String::as_str).collect();
            select_servers_push_refs_and_generate_pr_or_pr_update_event(
                &client,
                &git_repo,
                &repo_ref,
                tip,
                first_commit,
                merge_base.as_ref(),
                &proposal_metadata,
                &user_ref,
                root_proposal.as_ref(),
                &cover_letter_title_description,
                &signer,
                &crate::output::term(),
                &push_options_refs,
                args.git_server.as_deref(),
            )
            .await?
        }
    } else {
        let ordering_reference = if let Some(root) = root_proposal.as_ref() {
            get_all_proposal_patch_pr_pr_update_events_from_cache(
                git_repo.get_path()?,
                &repo_ref,
                &root.id,
            )
            .await
            .ok()
            .and_then(|events| ngit::event_ordering::latest_event(&events).cloned())
        } else {
            None
        };
        let events = generate_cover_letter_and_patch_events(
            cover_letter_title_description.clone(),
            &git_repo,
            &commits,
            &signer,
            &repo_ref,
            &root_proposal.as_ref().map(|e| e.id.to_string()),
            &mention_tags,
            ordering_reference.as_ref(),
        )
        .await?;

        println!(
            "posting {} patch{} {} a covering letter...",
            if cover_letter_title_description.is_none() {
                events.len()
            } else {
                events.len() - 1
            },
            if cover_letter_title_description.is_none() && events.len().eq(&1)
                || cover_letter_title_description.is_some() && events.len().eq(&2)
            {
                ""
            } else {
                "es"
            },
            if cover_letter_title_description.is_none() {
                "without"
            } else {
                "with"
            }
        );
        events
    };

    let _ = send_events(
        &client,
        Some(git_repo_path),
        events.clone(),
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
        false,
    )
    .await?;

    if crate::output::is_json() {
        let result_event = events.first().context("proposal generated no events")?;
        let entity = if as_pr { "pr" } else { "patch" };
        let action = if root_proposal.is_some() {
            "updated"
        } else {
            "created"
        };
        crate::output::set_event(action, entity, result_event.id, repo_ref.relays.first());
    }

    if root_proposal.is_none() {
        if let Some(event) = events.first() {
            let event_bech32 = if let Some(relay) = repo_ref.relays.first() {
                Nip19Event {
                    event_id: event.id,
                    relays: vec![relay.clone()],
                    author: None,
                    kind: None,
                }
                .to_bech32()?
            } else {
                event.id.to_bech32()?
            };
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in gitworkshop.dev: https://gitworkshop.dev/{event_bech32}",
                ))
            );
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in another client:  https://njump.me/{event_bech32}",
                ))
            );
        }
    }
    // TODO check if there is already a similarly named
    Ok(())
}

fn check_commits_are_suitable_for_proposal(
    cli: &Cli,
    first_commit_ahead: &[Sha1Hash],
    commits: &[Sha1Hash],
    behind: &[Sha1Hash],
    main_branch_name: &str,
    main_tip: &Sha1Hash,
    supports_explicit_base: bool,
) -> Result<()> {
    let base_guidance = if supports_explicit_base {
        "retry with --base <commit-or-ref> set to the commit before your proposed changes"
    } else {
        "select a different commit range or use --force to keep the selected commits"
    };
    // check proposal ahead of origin/main
    if first_commit_ahead.len().gt(&1) {
        if cli.interactive {
            if !Interactor::default().confirm(
                PromptConfirmParms::default()
                    .with_prompt(
                        format!("proposal builds on a commit {} ahead of '{main_branch_name}' - do you want to continue?", first_commit_ahead.len() - 1)
                    )
                    .with_default(false)
            ).context("failed to get confirmation response from interactor confirm")? {
                bail!("aborting; {base_guidance}");
            }
        } else if !cli.force {
            bail!(
                "proposal builds on a commit {} ahead of '{}'. {base_guidance}",
                first_commit_ahead.len() - 1,
                main_branch_name
            );
        }
    }

    // check if a selected commit is already in origin
    if commits.iter().any(|c| c.eq(main_tip)) {
        if cli.interactive {
            if !Interactor::default().confirm(
                PromptConfirmParms::default()
                    .with_prompt(
                        format!("proposal contains commit(s) already in  '{main_branch_name}'. proceed anyway?")
                    )
                    .with_default(false)
            ).context("failed to get confirmation response from interactor confirm")? {
                bail!("aborting; {base_guidance}");
            }
        } else if !cli.force {
            bail!("proposal contains commit(s) already in '{main_branch_name}'. {base_guidance}");
        }
    }
    // check proposal isn't behind origin/main
    else if !behind.is_empty() {
        if cli.interactive {
            if !Interactor::default().confirm(
                PromptConfirmParms::default()
                    .with_prompt(
                        format!("proposal is {} behind '{main_branch_name}'. consider rebasing before submission. proceed anyway?", behind.len())
                    )
                    .with_default(false)
            ).context("failed to get confirmation response from interactor confirm")? {
                bail!("aborting so commits can be rebased; alternatively, {base_guidance}");
            }
        } else if !cli.force {
            bail!(
                "proposal is {} behind '{}'. rebase first, or {base_guidance}",
                behind.len(),
                main_branch_name
            );
        }
    }
    Ok(())
}

fn choose_commits(git_repo: &Repo, proposed_commits: Vec<Sha1Hash>) -> Result<Vec<Sha1Hash>> {
    let mut proposed_commits = if proposed_commits.len().gt(&10) {
        vec![]
    } else {
        proposed_commits
    };

    let tip_of_head = git_repo.get_tip_of_branch(&git_repo.get_checked_out_branch_name()?)?;
    let most_recent_commit = proposed_commits.first().unwrap_or(&tip_of_head);

    let mut last_15_commits = vec![*most_recent_commit];

    while last_15_commits.len().lt(&15) {
        if let Ok(parent_commit) = git_repo.get_commit_parent(last_15_commits.last().unwrap()) {
            last_15_commits.push(parent_commit);
        } else {
            break;
        }
    }

    let term = console::Term::stderr();
    let mut printed_error_line = false;

    let selected_commits = 'outer: loop {
        let selected = Interactor::default().multi_choice(
            PromptMultiChoiceParms::default()
                .with_prompt("select commits for proposal")
                .dont_report()
                .with_choices(
                    last_15_commits
                        .iter()
                        .map(|h| summarise_commit_for_selection(git_repo, h).unwrap())
                        .collect(),
                )
                .with_defaults(
                    last_15_commits
                        .iter()
                        .map(|h| proposed_commits.iter().any(|c| c.eq(h)))
                        .collect(),
                ),
        )?;
        proposed_commits = selected.iter().map(|i| last_15_commits[*i]).collect();

        if printed_error_line {
            term.clear_last_lines(1)?;
        }

        if proposed_commits.is_empty() {
            term.write_line("no commits selected")?;
            printed_error_line = true;
            continue;
        }
        for (i, selected_i) in selected.iter().enumerate() {
            if i.gt(&0) && selected_i.ne(&(selected[i - 1] + 1)) {
                term.write_line("commits must be consecutive. try again.")?;
                printed_error_line = true;
                continue 'outer;
            }
        }

        break proposed_commits;
    };
    Ok(selected_commits)
}

fn summarise_commit_for_selection(git_repo: &Repo, commit: &Sha1Hash) -> Result<String> {
    let references = git_repo.get_refs(commit)?;
    let dim = Style::new().color256(247);
    let prefix = format!("({})", git_repo.get_commit_author(commit)?[0]);
    let references_string = if references.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            references
                .iter()
                .map(|r| format!("[{r}]"))
                .collect::<Vec<String>>()
                .join(" ")
        )
    };

    Ok(format!(
        "{} {}{} {}",
        dim.apply_to(prefix),
        git_repo.get_commit_message_summary(commit)?,
        Style::new().magenta().apply_to(references_string),
        dim.apply_to(commit.to_string().chars().take(7).collect::<String>(),),
    ))
}

async fn get_root_proposal_and_mentions_from_in_reply_to(
    git_repo_path: &Path,
    in_reply_to: &[String],
) -> Result<(Option<Event>, Vec<nostr::prelude::Tag>)> {
    let root_proposal = if let Some(first) = in_reply_to.first() {
        let root_tag =
            event_tag_from_nip19_or_hex(first, "in-reply-to", EventRefType::Root, true, false)?;
        if let Ok(Nip10Tag::Event { id: event_id, .. }) = Nip10Tag::try_from(root_tag) {
            let events = get_events_from_local_cache(
                git_repo_path,
                vec![nostr::prelude::Filter::new().id(event_id)],
            )
            .await?;

            if let Some(first) = events.iter().find(|e| e.id.eq(&event_id)) {
                if event_is_patch_set_root(first) || first.kind.eq(&KIND_PULL_REQUEST) {
                    Some(first.clone())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        return Ok((None, vec![]));
    };

    let mut mention_tags = vec![];
    for (i, reply_to) in in_reply_to.iter().enumerate() {
        if i.ne(&0) || root_proposal.is_none() {
            mention_tags.push(
                event_tag_from_nip19_or_hex(
                    reply_to,
                    "in-reply-to",
                    EventRefType::Quote,
                    true,
                    false,
                )
                .context(format!(
                    "{reply_to} in 'in-reply-to' not a valid nostr reference"
                ))?,
            );
        }
    }

    Ok((root_proposal, mention_tags))
}

// TODO
// - find profile
// - file relays
// - find repo events
// -
