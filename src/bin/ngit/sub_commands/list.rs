use std::{
    collections::HashSet,
    io::Write,
    ops::Add,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use ngit::{
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache,
    },
    fetch::fetch_from_git_server,
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, get_commit_id_from_patch,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, get_status, status_kinds, tag_value,
    },
    repo_ref::{RepoRef, is_grasp_server_in_list},
};
use nostr::{
    FromBech32, ToBech32,
    filter::{Alphabet, SingleLetterTag},
    nips::nip19::Nip19,
};
use nostr_sdk::Kind;

use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms},
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions, str_to_sha1},
    git_events::{
        commit_msg_from_patch_oneliner, event_is_revision_root, event_to_cover_letter,
        patch_supports_commit_ids,
    },
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

fn run_git_fetch(remote_name: &str) -> Result<()> {
    let verbose = ngit::client::is_verbose();
    if verbose {
        println!("fetching from {remote_name}...");
    }

    let spinner = if verbose {
        None
    } else {
        let pb = ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .unwrap()
                    .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈"),
            )
            .with_message(format!("Fetching from {remote_name}..."));
        pb.enable_steady_tick(Duration::from_millis(100));
        Some(pb)
    };

    let output = Command::new("git")
        .args(["fetch", remote_name])
        .stdout(if verbose {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .stderr(if verbose {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .output()
        .context("failed to run git fetch")?;

    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }

    if !output.status.success() {
        if !verbose {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                eprintln!("{stderr}");
            }
        }
        bail!(
            "git fetch {remote_name} exited with error: {}",
            output.status
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn launch(status: String, json: bool, id: Option<String>) -> Result<()> {
    if std::env::var("NGIT_INTERACTIVE_MODE").is_ok() {
        return launch_interactive().await;
    }

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    let nostr_remote = git_repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await
        .ok()
        .flatten();

    if let Some((remote_name, _)) = &nostr_remote {
        if std::env::var("NGITTEST").is_ok() {
            fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
        } else {
            run_git_fetch(remote_name)?;
        }
    } else {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;
    if proposals_and_revisions.is_empty() {
        println!("no proposals found... create one? try `ngit send`");
        return Ok(());
    }

    let statuses: Vec<nostr::Event> = {
        let mut statuses = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
                nostr::Filter::default()
                    .custom_tags(
                        SingleLetterTag::uppercase(Alphabet::E),
                        proposals_and_revisions.iter().map(|e| e.id),
                    )
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };

    let mut open_proposals: Vec<&nostr::Event> = vec![];
    let mut draft_proposals: Vec<&nostr::Event> = vec![];
    let mut closed_proposals: Vec<&nostr::Event> = vec![];
    let mut applied_proposals: Vec<&nostr::Event> = vec![];

    let proposals: Vec<nostr::Event> = proposals_and_revisions
        .iter()
        .filter(|e| !event_is_revision_root(e))
        .cloned()
        .collect();

    for proposal in &proposals {
        let status_kind = get_status(proposal, &repo_ref, &statuses, &proposals);
        if status_kind.eq(&Kind::GitStatusOpen) {
            open_proposals.push(proposal);
        } else if status_kind.eq(&Kind::GitStatusClosed) {
            closed_proposals.push(proposal);
        } else if status_kind.eq(&Kind::GitStatusDraft) {
            draft_proposals.push(proposal);
        } else if status_kind.eq(&Kind::GitStatusApplied) {
            applied_proposals.push(proposal);
        }
    }

    let status_filter: HashSet<&str> = status.split(',').map(str::trim).collect();

    let filtered_proposals: Vec<(&nostr::Event, Kind)> = proposals
        .iter()
        .filter_map(|p| {
            let status_kind = get_status(p, &repo_ref, &statuses, &proposals);
            let status_str = match status_kind {
                Kind::GitStatusOpen => "open",
                Kind::GitStatusDraft => "draft",
                Kind::GitStatusClosed => "closed",
                Kind::GitStatusApplied => "applied",
                _ => "unknown",
            };
            if status_filter.contains(status_str) || status_filter.contains("unknown") {
                Some((p, status_kind))
            } else {
                None
            }
        })
        .collect();

    if let Some(ref event_id_or_nevent) = id {
        return show_proposal_details(&filtered_proposals, &repo_ref, event_id_or_nevent, json);
    }

    if json {
        output_json(&filtered_proposals, &repo_ref)?;
    } else {
        output_table(&filtered_proposals, &repo_ref, &status);
    }

    Ok(())
}

fn status_kind_to_str(kind: Kind) -> &'static str {
    match kind {
        Kind::GitStatusOpen => "open",
        Kind::GitStatusDraft => "draft",
        Kind::GitStatusClosed => "closed",
        Kind::GitStatusApplied => "applied",
        _ => "unknown",
    }
}

fn output_table(proposals: &[(&nostr::Event, Kind)], _repo_ref: &RepoRef, status_filter: &str) {
    if proposals.is_empty() {
        println!("No proposals found matching status: {status_filter}");
        return;
    }

    println!("{:<66} {:<8} TITLE", "ID", "STATUS");
    for (proposal, status_kind) in proposals {
        let id = proposal.id.to_string();
        let status = status_kind_to_str(*status_kind);
        let title = if let Ok(cl) = event_to_cover_letter(proposal) {
            cl.title
        } else if let Ok(msg) = tag_value(proposal, "description") {
            msg.split('\n').collect::<Vec<&str>>()[0].to_string()
        } else {
            proposal.id.to_string()
        };
        println!("{id:<66} {status:<8} {title}");
    }

    println!();
    println!("--status {status_filter}");
    println!("{}", console::style("To view:     ngit list <id>").yellow());
    println!(
        "{}",
        console::style("To checkout: ngit checkout <id>").yellow()
    );
    println!(
        "{}",
        console::style("To apply:    ngit apply <id>").yellow()
    );
}

fn output_json(proposals: &[(&nostr::Event, Kind)], _repo_ref: &RepoRef) -> Result<()> {
    let json_output: Vec<serde_json::Value> = proposals
        .iter()
        .map(|(proposal, status_kind)| {
            let id = proposal.id.to_string();
            let status = status_kind_to_str(*status_kind).to_string();
            let (title, author, branch) = if let Ok(cl) = event_to_cover_letter(proposal) {
                (
                    cl.title.clone(),
                    proposal.pubkey.to_bech32().unwrap_or_default(),
                    cl.get_branch_name_with_pr_prefix_and_shorthand_id()
                        .unwrap_or_default(),
                )
            } else {
                let title = tag_value(proposal, "description").map_or_else(
                    |_| proposal.id.to_string(),
                    |d| d.split('\n').collect::<Vec<&str>>()[0].to_string(),
                );
                (
                    title,
                    proposal.pubkey.to_bech32().unwrap_or_default(),
                    String::new(),
                )
            };
            serde_json::json!({
                "id": id,
                "status": status,
                "title": title,
                "author": author,
                "branch": branch
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&json_output)?);
    Ok(())
}

fn show_proposal_details(
    proposals: &[(&nostr::Event, Kind)],
    _repo_ref: &RepoRef,
    event_id_or_nevent: &str,
    json: bool,
) -> Result<()> {
    let target_id = if event_id_or_nevent.starts_with("nevent") {
        let nip19 = Nip19::from_bech32(event_id_or_nevent).context("failed to parse nevent")?;
        match nip19 {
            Nip19::EventId(id) => id,
            Nip19::Event(event) => event.event_id,
            _ => bail!("invalid nevent format"),
        }
    } else {
        nostr::EventId::from_hex(event_id_or_nevent).context("failed to parse event id")?
    };

    let (proposal, status_kind) = proposals
        .iter()
        .find(|(p, _)| p.id == target_id)
        .context("proposal not found")?;

    let cover_letter = event_to_cover_letter(proposal)
        .context("failed to extract proposal details from proposal root event")?;

    if json {
        let json_output = serde_json::json!({
            "id": proposal.id.to_string(),
            "status": status_kind_to_str(*status_kind),
            "title": cover_letter.title,
            "author": proposal.pubkey.to_bech32().unwrap_or_default(),
            "branch": cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
            "description": cover_letter.description,
        });
        println!("{}", serde_json::to_string_pretty(&json_output)?);
        return Ok(());
    }

    println!("Title: {}", cover_letter.title);
    println!(
        "Author: {}",
        proposal.pubkey.to_bech32().unwrap_or_default()
    );
    println!("Status: {}", status_kind_to_str(*status_kind));
    println!(
        "Branch: {}",
        cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?
    );

    if !cover_letter.description.is_empty() {
        println!();
        println!("Description:");
        for line in cover_letter.description.lines() {
            println!("  {line}");
        }
    }

    println!();
    println!(
        "{}",
        console::style(format!("To checkout: ngit checkout {}", proposal.id)).yellow()
    );
    println!(
        "{}",
        console::style(format!("To apply:    ngit apply {}", proposal.id)).yellow()
    );

    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn launch_interactive() -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    // TODO: check for empty repo
    // TODO: check for existing maintaiers file
    // TODO: check for other claims

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    let nostr_remote = git_repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await
        .ok()
        .flatten();

    if let Some((remote_name, _)) = &nostr_remote {
        if std::env::var("NGITTEST").is_ok() {
            fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
        } else {
            run_git_fetch(remote_name)?;
        }
    } else {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;
    if proposals_and_revisions.is_empty() {
        println!("no proposals found... create one? try `ngit send`");
        return Ok(());
    }

    let statuses: Vec<nostr::Event> = {
        let mut statuses = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
                nostr::Filter::default()
                    .custom_tags(
                        SingleLetterTag::uppercase(Alphabet::E),
                        proposals_and_revisions.iter().map(|e| e.id),
                    )
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };

    let mut open_proposals: Vec<&nostr::Event> = vec![];
    let mut draft_proposals: Vec<&nostr::Event> = vec![];
    let mut closed_proposals: Vec<&nostr::Event> = vec![];
    let mut applied_proposals: Vec<&nostr::Event> = vec![];

    let proposals: Vec<nostr::Event> = proposals_and_revisions
        .iter()
        .filter(|e|
            // If we wanted to treat to list Pull Requests that revise a Patch we would do this:
            // e.kind.eq(&KIND_PULL_REQUEST) ||
            !event_is_revision_root(e))
        .cloned()
        .collect();

    for proposal in &proposals {
        let status = get_status(proposal, &repo_ref, &statuses, &proposals);
        if status.eq(&Kind::GitStatusOpen) {
            open_proposals.push(proposal);
        } else if status.eq(&Kind::GitStatusClosed) {
            closed_proposals.push(proposal);
        } else if status.eq(&Kind::GitStatusDraft) {
            draft_proposals.push(proposal);
        } else if status.eq(&Kind::GitStatusApplied) {
            applied_proposals.push(proposal);
        }
    }

    let mut selected_status = Kind::GitStatusOpen;

    loop {
        let proposals_for_status = if selected_status == Kind::GitStatusOpen {
            &open_proposals
        } else if selected_status == Kind::GitStatusDraft {
            &draft_proposals
        } else if selected_status == Kind::GitStatusClosed {
            &closed_proposals
        } else if selected_status == Kind::GitStatusApplied {
            &applied_proposals
        } else {
            &open_proposals
        };

        let prompt = if proposals.len().eq(&open_proposals.len()) {
            "all proposals"
        } else if selected_status == Kind::GitStatusOpen {
            if open_proposals.is_empty() {
                "proposals menu"
            } else {
                "open proposals"
            }
        } else if selected_status == Kind::GitStatusDraft {
            "draft proposals"
        } else if selected_status == Kind::GitStatusClosed {
            "closed proposals"
        } else {
            "applied proposals"
        };

        let mut choices: Vec<String> = proposals_for_status
            .iter()
            .map(|e| {
                if let Ok(cl) = event_to_cover_letter(e) {
                    cl.title
                } else if let Ok(msg) = tag_value(e, "description") {
                    msg.split('\n').collect::<Vec<&str>>()[0].to_string()
                } else {
                    e.id.to_string()
                }
            })
            .collect();

        if !selected_status.eq(&Kind::GitStatusOpen) && open_proposals.len().gt(&0) {
            choices.push(format!("({}) Open proposals...", open_proposals.len()));
        }
        if !selected_status.eq(&Kind::GitStatusDraft) && draft_proposals.len().gt(&0) {
            choices.push(format!("({}) Draft proposals...", draft_proposals.len()));
        }
        if !selected_status.eq(&Kind::GitStatusClosed) && closed_proposals.len().gt(&0) {
            choices.push(format!("({}) Closed proposals...", closed_proposals.len()));
        }
        if !selected_status.eq(&Kind::GitStatusApplied) && applied_proposals.len().gt(&0) {
            choices.push(format!(
                "({}) Applied proposals...",
                applied_proposals.len()
            ));
        }

        let selected_index = Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt(prompt)
                .with_default(0)
                .with_choices(choices.clone()),
        )?;

        if (selected_index + 1).gt(&proposals_for_status.len()) {
            if choices[selected_index].contains("Open") {
                selected_status = Kind::GitStatusOpen;
            } else if choices[selected_index].contains("Draft") {
                selected_status = Kind::GitStatusDraft;
            } else if choices[selected_index].contains("Closed") {
                selected_status = Kind::GitStatusClosed;
            } else if choices[selected_index].contains("Applied") {
                selected_status = Kind::GitStatusApplied;
            }
            continue;
        }

        let cover_letter = event_to_cover_letter(proposals_for_status[selected_index])
            .context("failed to extract proposal details from proposal root event")?;

        let commits_events: Vec<nostr::Event> =
            get_all_proposal_patch_pr_pr_update_events_from_cache(
                git_repo_path,
                &repo_ref,
                &proposals_for_status[selected_index].id,
            )
            .await?;

        let Ok(most_recent_proposal_patch_chain_or_pr_or_pr_update) =
            get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events.clone())
        else {
            if Interactor::default().confirm(
                PromptConfirmParms::default()
                    .with_default(true)
                    .with_prompt(
                        "failed to find any PR or patch events on this proposal. choose another proposal?",
                    ),
            )? {
                continue;
            }
            return Ok(());
        };
        // for commit in &most_recent_proposal_patch_chain {
        //     println!("recent_event: {:?}", commit.as_json());
        // }
        if most_recent_proposal_patch_chain_or_pr_or_pr_update
            .iter()
            .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind))
        {
            let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
            let local_branch_tip = git_repo.get_tip_of_branch(&branch_name).ok();
            let proposal_tip_event = most_recent_proposal_patch_chain_or_pr_or_pr_update
            .first()
            .context("most_recent_proposal_patch_chain_or_pr_or_pr_update will always contain a event with c tag")?;
            let proposal_tip = tag_value(proposal_tip_event, "c")?;

            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        if let Some(local_branch_tip) = local_branch_tip {
                            if local_branch_tip.to_string() == proposal_tip {
                                format!("checkout up-to-date proposal branch '{branch_name}'")
                            } else {
                                format!("checkout proposal branch and pull changes '{branch_name}'")
                            }
                        } else {
                            format!("create and checkout as branch '{branch_name}'")
                        },
                        "back to proposals".to_string(),
                    ]),
            )? {
                0 => {
                    if let Some(local_branch_tip) = local_branch_tip {
                        git_repo
                            .checkout(&branch_name)
                            .context("cannot checkout existing proposal branch")?;
                        if local_branch_tip.to_string() == proposal_tip {
                            println!("checked out up-to-date proposal branch '{branch_name}'");
                            return Ok(());
                        }
                        if git_repo.does_commit_exist(&proposal_tip)? {
                            println!("checked out proposal branch and updated tip '{branch_name}'");
                            return Ok(());
                        }
                    }
                    fetch_oid_for_from_servers_for_pr(
                        &proposal_tip,
                        &git_repo,
                        &repo_ref,
                        proposal_tip_event,
                    )?;
                    git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
                    git_repo.checkout(&branch_name)?;
                    if local_branch_tip.is_some() {
                        println!("created and checked out proposal branch '{branch_name}'");
                    } else {
                        println!("checked out proposal branch and pulled updates '{branch_name}'");
                    }
                    return Ok(());
                }
                1 => {
                    continue;
                }
                _ => {
                    bail!("unexpected choice")
                }
            }
        }

        let binding_patch_text_ref = format!(
            "{} commits",
            most_recent_proposal_patch_chain_or_pr_or_pr_update.len()
        );
        let patch_text_ref = if most_recent_proposal_patch_chain_or_pr_or_pr_update
            .len()
            .gt(&1)
        {
            binding_patch_text_ref.as_str()
        } else {
            "1 commit"
        };

        let no_support_for_patches_as_branch = most_recent_proposal_patch_chain_or_pr_or_pr_update
            .iter()
            .any(|event| !patch_supports_commit_ids(event));

        if no_support_for_patches_as_branch {
            println!("{patch_text_ref}");
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        "learn why this proposals can't be checked out".to_string(),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    println!(
                        "Some proposals are posted as patch without listing a parent commit\n"
                    );
                    println!(
                        "they are not anchored against a particular state of the code base like a standard patch or a pull request can be\n"
                    );
                    println!(
                        "they are designed to reviewed by studying the diff (in a tool like gitworkshop.dev) and if acceptable by a maintainer, applied to the latest version of master with any conflicts resolved as the do so\n"
                    );
                    println!(
                        "this has proven to be a smoother workflow for large scale projects with a high frequency of changes, even when patches are exchanged via email\n"
                    );
                    println!(
                        "by default ngit posts proposals with a parent commit so either workflow can be used"
                    );
                    Interactor::default().choice(
                        PromptChoiceParms::default()
                            .with_default(0)
                            .with_choices(vec!["back".to_string()]),
                    )?;
                    continue;
                }
                1 => {
                    launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update)
                }
                2 => save_patches_to_dir(
                    most_recent_proposal_patch_chain_or_pr_or_pr_update,
                    &git_repo,
                ),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let branch_exists = git_repo
            .get_local_branch_names()
            .context("gitlib2 will not show a list of local branch names")?
            .iter()
            .any(|n| {
                n.eq(&cover_letter
                    .get_branch_name_with_pr_prefix_and_shorthand_id()
                    .unwrap())
            });

        let checked_out_proposal_branch = git_repo
            .get_checked_out_branch_name()?
            .eq(&cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?);

        let proposal_base_commit = str_to_sha1(&tag_value(
            most_recent_proposal_patch_chain_or_pr_or_pr_update
                .last()
                .context(
                    "there should be at least one patch as we have already checked for this",
                )?,
            "parent-commit",
        )?)
        .context("failed to get valid parent commit id from patch")?;

        let (main_branch_name, master_tip) = git_repo.get_main_or_master_branch()?;

        if !git_repo.does_commit_exist(&proposal_base_commit.to_string())? {
            println!("your '{main_branch_name}' branch may not be up-to-date.");
            println!("the proposal parent commit doesnt exist in your local repository.");
            return match Interactor::default().choice(PromptChoiceParms::default().with_default(0).with_choices(
                vec![
                    format!(
                        "manually run `git pull` on '{main_branch_name}' and select proposal again"
                    ),
                    format!("apply to current branch with `git am`"),
                    format!("download to ./patches"),
                    "back".to_string(),
                ],
            ))? {
                0 | 3 => continue,
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain_or_pr_or_pr_update, &git_repo),
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let proposal_tip = str_to_sha1(
            &get_commit_id_from_patch(
                most_recent_proposal_patch_chain_or_pr_or_pr_update
                    .first()
                    .context(
                        "there should be at least one patch as we have already checked for this",
                    )?,
            )
            .context("failed to get valid commit_id from patch")?,
        )
        .context("failed to get valid commit_id from patch")?;

        let (_, proposal_behind_main) =
            git_repo.get_commits_ahead_behind(&master_tip, &proposal_base_commit)?;

        // branch doesnt exist
        if !branch_exists {
            return match Interactor::default()
                .choice(PromptChoiceParms::default().with_default(0).with_choices(vec![
                format!(
                    "create and checkout proposal branch ({} ahead {} behind '{main_branch_name}')",
                    most_recent_proposal_patch_chain_or_pr_or_pr_update.len(),
                    proposal_behind_main.len(),
                ),
                format!("apply to current branch with `git am`"),
                format!("download to ./patches"),
                "back".to_string(),
            ]))? {
                0 => {
                    check_clean(&git_repo)?;
                    let _ = git_repo
                        .apply_patch_chain(
                            &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                            most_recent_proposal_patch_chain_or_pr_or_pr_update,
                        )
                        .context("failed to apply patch chain")?;

                    println!(
                        "checked out proposal as '{}' branch",
                        cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?
                    );
                    Ok(())
                }
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain_or_pr_or_pr_update, &git_repo),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let local_branch_tip = git_repo
            .get_tip_of_branch(&cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?)?;

        // up-to-date
        if proposal_tip.eq(&local_branch_tip) {
            if checked_out_proposal_branch {
                println!("branch checked out and up-to-date");
                return match Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_default(0)
                        .with_choices(vec!["exit".to_string(), "back".to_string()]),
                )? {
                    0 => Ok(()),
                    1 => continue,
                    _ => {
                        bail!("unexpected choice")
                    }
                };
            }

            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!(
                            "checkout proposal branch ({} ahead {} behind '{main_branch_name}')",
                            most_recent_proposal_patch_chain_or_pr_or_pr_update.len(),
                            proposal_behind_main.len(),
                        ),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    check_clean(&git_repo)?;
                    git_repo.checkout(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                    )?;
                    println!(
                        "checked out proposal as '{}' branch",
                        cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?
                    );
                    Ok(())
                }
                1 => {
                    launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update)
                }
                2 => save_patches_to_dir(
                    most_recent_proposal_patch_chain_or_pr_or_pr_update,
                    &git_repo,
                ),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let (local_ahead_of_main, local_beind_main) =
            git_repo.get_commits_ahead_behind(&master_tip, &local_branch_tip)?;

        // new appendments to proposal
        if let Some(index) = most_recent_proposal_patch_chain_or_pr_or_pr_update
            .iter()
            .position(|patch| {
                get_commit_id_from_patch(patch)
                    .unwrap_or_default()
                    .eq(&local_branch_tip.to_string())
            })
        {
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!("checkout proposal branch and apply {} appendments", &index,),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    check_clean(&git_repo)?;
                    git_repo.checkout(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                    )?;
                    let _ = git_repo
                        .apply_patch_chain(
                            &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                            most_recent_proposal_patch_chain_or_pr_or_pr_update,
                        )
                        .context("failed to apply patch chain")?;
                    println!(
                        "checked out proposal branch and applied {} appendments ({} ahead {} behind '{main_branch_name}')",
                        &index,
                        local_ahead_of_main.len().add(&index),
                        local_beind_main.len(),
                    );
                    Ok(())
                }
                1 => {
                    launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update)
                }
                2 => save_patches_to_dir(
                    most_recent_proposal_patch_chain_or_pr_or_pr_update,
                    &git_repo,
                ),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        // new proposal revision / rebase
        // tip of local in proposal history (new, amended or rebased version but no
        // local changes)
        if commits_events.iter().any(|patch| {
            get_commit_id_from_patch(patch)
                .unwrap_or_default()
                .eq(&local_branch_tip.to_string())
        }) {
            println!(
                "updated proposal available ({} ahead {} behind '{main_branch_name}'). existing version is {} ahead {} behind '{main_branch_name}'",
                most_recent_proposal_patch_chain_or_pr_or_pr_update.len(),
                proposal_behind_main.len(),
                local_ahead_of_main.len(),
                local_beind_main.len(),
            );
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!("checkout and overwrite existing proposal branch"),
                        format!("checkout existing outdated proposal branch"),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    check_clean(&git_repo)?;
                    git_repo.create_branch_at_commit(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                        &proposal_base_commit.to_string(),
                    )?;
                    git_repo.checkout(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                    )?;
                    let chain_length = most_recent_proposal_patch_chain_or_pr_or_pr_update.len();
                    let _ = git_repo
                        .apply_patch_chain(
                            &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                            most_recent_proposal_patch_chain_or_pr_or_pr_update,
                        )
                        .context("failed to apply patch chain")?;
                    println!(
                        "checked out new version of proposal ({} ahead {} behind '{main_branch_name}'), replacing old version ({} ahead {} behind '{main_branch_name}')",
                        chain_length,
                        proposal_behind_main.len(),
                        local_ahead_of_main.len(),
                        local_beind_main.len(),
                    );
                    Ok(())
                }
                1 => {
                    check_clean(&git_repo)?;
                    git_repo.checkout(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                    )?;
                    println!(
                        "checked out old proposal in existing branch ({} ahead {} behind '{main_branch_name}')",
                        local_ahead_of_main.len(),
                        local_beind_main.len(),
                    );
                    Ok(())
                }
                2 => {
                    launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update)
                }
                3 => save_patches_to_dir(
                    most_recent_proposal_patch_chain_or_pr_or_pr_update,
                    &git_repo,
                ),
                4 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }
        // tip of proposal in branch in history (local appendments made to up-to-date
        // proposal)
        else if git_repo.ancestor_of(&local_branch_tip, &proposal_tip)? {
            let (local_ahead_of_proposal, _) = git_repo
                .get_commits_ahead_behind(&proposal_tip, &local_branch_tip)
                .context(
                    "failed to get commits ahead behind for propsal_top and local_branch_tip",
                )?;

            println!(
                "local proposal branch exists with {} unpublished commits on top of the most up-to-date version of the proposal ({} ahead {} behind '{main_branch_name}')",
                local_ahead_of_proposal.len(),
                local_ahead_of_main.len(),
                proposal_behind_main.len(),
            );
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!(
                            "checkout proposal branch with {} unpublished commits",
                            local_ahead_of_proposal.len(),
                        ),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    git_repo.checkout(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                    )?;
                    println!(
                        "checked out proposal branch with {} unpublished commits ({} ahead {} behind '{main_branch_name}')",
                        local_ahead_of_proposal.len(),
                        local_ahead_of_main.len(),
                        proposal_behind_main.len(),
                    );
                    Ok(())
                }
                1 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        println!("you have an amended/rebase version the proposal that is unpublished");
        // user probably has a unpublished amended or rebase version of the latest
        // proposal version
        // if tip of proposal commits exist (were once part of branch but have been
        // amended and git clean up job hasn't removed them)
        if git_repo.does_commit_exist(&proposal_tip.to_string())? {
            println!(
                "you have previously applied the latest version of the proposal ({} ahead {} behind '{main_branch_name}') but your local proposal branch has amended or rebased it ({} ahead {} behind '{main_branch_name}')",
                most_recent_proposal_patch_chain_or_pr_or_pr_update.len(),
                proposal_behind_main.len(),
                local_ahead_of_main.len(),
                local_beind_main.len(),
            );
        }
        // user probably has a unpublished amended or rebase version of an older
        // proposal version
        else {
            println!(
                "your local proposal branch ({} ahead {} behind '{main_branch_name}') has conflicting changes with the latest published proposal ({} ahead {} behind '{main_branch_name}')",
                local_ahead_of_main.len(),
                local_beind_main.len(),
                most_recent_proposal_patch_chain_or_pr_or_pr_update.len(),
                proposal_behind_main.len(),
            );

            println!(
                "its likely that you have rebased / amended an old proposal version because git has no record of the latest proposal commit."
            );
            println!(
                "it is possible that you have been working off the latest version and git has delete this commit as part of a clean up"
            );
        }
        println!("to view the latest proposal but retain your changes:");
        println!("  1) create a new branch off the tip commit of this one to store your changes");
        println!("  2) run `ngit list` and checkout the latest published version of this proposal");

        println!("if you are confident in your changes consider running `ngit push --force`");

        return match Interactor::default().choice(
            PromptChoiceParms::default()
                .with_default(0)
                .with_choices(vec![
                    format!("checkout local branch with unpublished changes"),
                    format!("discard unpublished changes and checkout new revision",),
                    format!("apply to current branch with `git am`"),
                    format!("download to ./patches"),
                    "back".to_string(),
                ]),
        )? {
            0 => {
                check_clean(&git_repo)?;
                git_repo
                    .checkout(&cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?)?;
                println!(
                    "checked out old proposal in existing branch ({} ahead {} behind '{main_branch_name}')",
                    local_ahead_of_main.len(),
                    local_beind_main.len(),
                );
                Ok(())
            }
            1 => {
                check_clean(&git_repo)?;
                git_repo.create_branch_at_commit(
                    &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                    &proposal_base_commit.to_string(),
                )?;
                let chain_length = most_recent_proposal_patch_chain_or_pr_or_pr_update.len();
                let _ = git_repo
                    .apply_patch_chain(
                        &cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?,
                        most_recent_proposal_patch_chain_or_pr_or_pr_update,
                    )
                    .context("failed to apply patch chain")?;

                git_repo
                    .checkout(&cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?)?;
                println!(
                    "checked out latest version of proposal ({} ahead {} behind '{main_branch_name}'), replacing unpublished version ({} ahead {} behind '{main_branch_name}')",
                    chain_length,
                    proposal_behind_main.len(),
                    local_ahead_of_main.len(),
                    local_beind_main.len(),
                );
                Ok(())
            }
            2 => launch_git_am_with_patches(most_recent_proposal_patch_chain_or_pr_or_pr_update),
            3 => save_patches_to_dir(
                most_recent_proposal_patch_chain_or_pr_or_pr_update,
                &git_repo,
            ),
            4 => continue,
            _ => {
                bail!("unexpected choice")
            }
        };
    }
}

fn fetch_oid_for_from_servers_for_pr(
    oid: &str,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    pr_or_pr_update_event: &nostr::Event,
) -> Result<()> {
    let git_servers = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = vec![];
        for tag in pr_or_pr_update_event.tags.as_slice() {
            if tag.kind().eq(&nostr::event::TagKind::Clone) {
                for clone_url in tag.as_slice().iter().skip(1) {
                    seen.insert(clone_url.clone());
                }
            }
        }
        for server in &repo_ref.git_server {
            if seen.insert(server.clone()) {
                out.push(server.clone());
            }
        }
        out
    };

    let mut errors = vec![];
    let term = console::Term::stderr();

    for git_server_url in &git_servers {
        if let Err(error) = fetch_from_git_server(
            git_repo,
            &[oid.to_string()],
            git_server_url,
            &repo_ref.to_nostr_git_url(&None),
            &term,
            is_grasp_server_in_list(git_server_url, &repo_ref.grasp_servers()),
        ) {
            errors.push(error);
        } else {
            println!("fetched proposal git data from {git_server_url}");
            break;
        }
    }
    if !git_repo.does_commit_exist(oid)? {
        bail!(
            "cannot find proposal git data from proposal git server hint or repository git servers"
        )
    }
    Ok(())
}

fn launch_git_am_with_patches(mut patches: Vec<nostr::Event>) -> Result<()> {
    println!("applying to current branch with `git am`");
    // TODO: add PATCH x/n to appended patches
    patches.reverse();

    let mut am = std::process::Command::new("git")
        .arg("am")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to spawn git am")?;

    let stdin = am
        .stdin
        .as_mut()
        .context("git am process failed to take stdin")?;

    for patch in patches {
        stdin
            .write(format!("{}\n\n", patch.content).as_bytes())
            .context("failed to write patch content into git am stdin buffer")?;
    }
    stdin.flush()?;
    let output = am
        .wait_with_output()
        .context("failed to read git am stdout")?;
    print!("{:?}", output.stdout);
    Ok(())
}

fn event_id_extra_shorthand(event: &nostr::Event) -> String {
    event.id.to_string()[..5].to_string()
}

fn save_patches_to_dir(mut patches: Vec<nostr::Event>, git_repo: &Repo) -> Result<()> {
    // TODO: add PATCH x/n to appended patches
    patches.reverse();
    let path = git_repo.get_path()?.join("patches");
    std::fs::create_dir_all(&path)?;
    let id = event_id_extra_shorthand(
        patches
            .first()
            .context("there must be at least one patch to save")?,
    );
    for (i, patch) in patches.iter().enumerate() {
        let path = path.join(format!(
            "{}-{:0>4}-{}.patch",
            &id,
            i.add(&1),
            commit_msg_from_patch_oneliner(patch)?
        ));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .context("open new patch file with write and truncate options")?;
        file.write_all(patch.content.as_bytes())?;
        file.write_all("\n\n".as_bytes())?;
        file.flush()?;
    }
    println!("created {} patch files in ./patches/{id}-*", patches.len());
    Ok(())
}

fn check_clean(git_repo: &Repo) -> Result<()> {
    if git_repo.has_outstanding_changes()? {
        bail!(
            "failed to pull proposal branch when repository is not clean. discard or stash (un)staged changes and try again."
        );
    }
    Ok(())
}
