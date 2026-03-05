use std::{
    collections::HashSet,
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
        get_pr_tip_event_or_most_recent_patch_with_ancestors, tag_value,
    },
    repo_ref::{RepoRef, is_grasp_server_in_list},
};
use nostr::nips::nip19::Nip19;
use nostr_sdk::{EventId, FromBech32};

use crate::{
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions, str_to_sha1},
    git_events::event_to_cover_letter,
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

pub async fn launch(id: &str, force: bool, offline: bool) -> Result<()> {
    let event_id = parse_event_id(id)?;

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    let nostr_remote = git_repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await
        .ok()
        .flatten();

    if !offline {
        if let Some((remote_name, _)) = &nostr_remote {
            run_git_fetch(remote_name)?;
        } else {
            fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
        }
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal = proposals_and_revisions
        .iter()
        .find(|e| e.id == event_id)
        .context(format!(
            "proposal with id {} not found in cache",
            event_id.to_hex()
        ))?;

    let cover_letter = event_to_cover_letter(proposal)
        .context("failed to extract proposal details from proposal root event")?;

    let commits_events: Vec<nostr::Event> = get_all_proposal_patch_pr_pr_update_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal.id,
    )
    .await?;

    let most_recent_proposal_patch_chain_or_pr_or_pr_update =
        get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events.clone())
            .context("failed to find any PR or patch events on this proposal")?;

    if most_recent_proposal_patch_chain_or_pr_or_pr_update
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind))
    {
        checkout_pr(
            &git_repo,
            &repo_ref,
            &cover_letter,
            &most_recent_proposal_patch_chain_or_pr_or_pr_update,
            nostr_remote.as_ref().map(|(name, _)| name.as_str()),
            force,
        )
    } else {
        checkout_patch(
            &git_repo,
            &cover_letter,
            &most_recent_proposal_patch_chain_or_pr_or_pr_update,
            nostr_remote.as_ref().map(|(name, _)| name.as_str()),
            force,
        )
    }
}

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

fn parse_event_id(id: &str) -> Result<EventId> {
    if let Ok(nip19) = Nip19::from_bech32(id) {
        match nip19 {
            Nip19::Event(e) => return Ok(e.event_id),
            Nip19::EventId(event_id) => return Ok(event_id),
            _ => {}
        }
    }
    if let Ok(event_id) = EventId::from_hex(id) {
        return Ok(event_id);
    }
    bail!("invalid event-id or nevent: {id}")
}

fn print_diverged_branch_help(branch_name: &str) {
    eprintln!(
        "{}",
        console::style(format!(
            "Branch '{branch_name}' has diverged from the published proposal."
        ))
        .yellow()
    );
    eprintln!(
        "{}",
        console::style(
            "This may be because you have local amendments, or the author force-pushed a new revision."
        )
        .yellow()
    );
    eprintln!(
        "{}",
        console::style("To overwrite local branch with the published version:").yellow()
    );
    eprintln!(
        "{}",
        console::style("  ngit pr checkout --force <id>").yellow()
    );
    eprintln!(
        "{}",
        console::style("To publish your local amendments as a new revision:").yellow()
    );
    eprintln!("{}", console::style("  ngit push --force").yellow());
}

fn checkout_pr(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    cover_letter: &crate::git_events::CoverLetter,
    most_recent_proposal_patch_chain_or_pr_or_pr_update: &[nostr::Event],
    nostr_remote_name: Option<&str>,
    force: bool,
) -> Result<()> {
    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
    let proposal_tip_event = most_recent_proposal_patch_chain_or_pr_or_pr_update
        .first()
        .context("most_recent_proposal_patch_chain_or_pr_or_pr_update will always contain an event with c tag")?;
    let proposal_tip = tag_value(proposal_tip_event, "c")?;
    let proposal_tip_sha1 = str_to_sha1(&proposal_tip)?;

    // Case 1: branch doesn't exist yet — create it.
    let Ok(local_branch_tip) = git_repo.get_tip_of_branch(&branch_name) else {
        if let Some(remote_name) = nostr_remote_name {
            let remote_branch = format!("{remote_name}/{branch_name}");
            if git_repo.get_tip_of_branch(&remote_branch).is_ok() {
                checkout_remote_branch_with_tracking(git_repo, remote_name, &branch_name)?;
                println!(
                    "checked out proposal branch '{branch_name}' with tracking to {remote_name}"
                );
                return Ok(());
            }
        }
        fetch_oid_for_from_servers_for_pr(&proposal_tip, git_repo, repo_ref, proposal_tip_event)?;
        git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
        git_repo.checkout(&branch_name)?;
        println!("created and checked out proposal branch '{branch_name}'");
        return Ok(());
    };

    // Case 2: up to date.
    if local_branch_tip.to_string() == proposal_tip {
        git_repo
            .checkout(&branch_name)
            .context("cannot checkout existing proposal branch")?;
        println!("checked out up-to-date proposal branch '{branch_name}'");
        return Ok(());
    }

    // Branch has a tracking remote — defer to git pull for updates.
    if git_repo.get_upstream_for_branch(&branch_name)?.is_some() {
        git_repo
            .checkout(&branch_name)
            .context("cannot checkout existing proposal branch")?;
        println!(
            "{}",
            console::style(format!(
                "Local branch '{branch_name}' is behind. Run git pull to update."
            ))
            .yellow()
        );
        return Ok(());
    }

    if git_repo.does_commit_exist(&proposal_tip)? {
        let local_is_ancestor_of_published =
            git_repo.ancestor_of(&proposal_tip_sha1, &local_branch_tip)?;
        let published_is_ancestor_of_local =
            git_repo.ancestor_of(&local_branch_tip, &proposal_tip_sha1)?;

        // Case 3: branch is behind — fast-forward.
        if local_is_ancestor_of_published {
            git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
            git_repo.checkout(&branch_name)?;
            println!("checked out proposal branch and updated tip '{branch_name}'");
            return Ok(());
        }

        // Case 4: local commits on top — check out without touching them.
        if published_is_ancestor_of_local {
            git_repo
                .checkout(&branch_name)
                .context("cannot checkout existing proposal branch")?;
            println!(
                "checked out proposal branch '{branch_name}' (local branch has unpublished commits on top)"
            );
            return Ok(());
        }
    }

    // Case 5 (and tip-not-found): diverged — require --force.
    if force {
        fetch_oid_for_from_servers_for_pr(&proposal_tip, git_repo, repo_ref, proposal_tip_event)?;
        git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
        git_repo.checkout(&branch_name)?;
        println!(
            "checked out proposal branch '{branch_name}' updated to published tip (overwrote diverged local branch)"
        );
        return Ok(());
    }

    git_repo
        .checkout(&branch_name)
        .context("cannot checkout existing proposal branch")?;
    print_diverged_branch_help(&branch_name);
    bail!(
        "branch '{branch_name}' has diverged from the published proposal; use --force to overwrite"
    )
}

#[allow(clippy::too_many_lines)]
fn checkout_patch(
    git_repo: &Repo,
    cover_letter: &crate::git_events::CoverLetter,
    most_recent_proposal_patch_chain_or_pr_or_pr_update: &[nostr::Event],
    nostr_remote_name: Option<&str>,
    force: bool,
) -> Result<()> {
    if git_repo.has_outstanding_changes()? {
        bail!("working directory is not clean. Discard or stash (un)staged changes and try again.");
    }

    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;

    // Case 1: branch doesn't exist yet — create and apply.
    let branch_exists = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|n| n.eq(&branch_name));

    if !branch_exists {
        if let Some(remote_name) = nostr_remote_name {
            let remote_branch = format!("{remote_name}/{branch_name}");
            if git_repo.get_tip_of_branch(&remote_branch).is_ok() {
                checkout_remote_branch_with_tracking(git_repo, remote_name, &branch_name)?;
                println!(
                    "checked out proposal branch '{branch_name}' with tracking to {remote_name}"
                );
                return Ok(());
            }
        }
        let _ = git_repo
            .apply_patch_chain(
                &branch_name,
                most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
            )
            .context("failed to apply patch chain")?;
        println!("checked out proposal as '{branch_name}' branch");
        return Ok(());
    }

    let local_branch_tip = git_repo.get_tip_of_branch(&branch_name)?;

    // Resolve the published tip commit id. If we can't (no commit tag), fall
    // through to apply_patch_chain which handles idempotency itself.
    let Ok(proposal_tip_str) = get_commit_id_from_patch(
        most_recent_proposal_patch_chain_or_pr_or_pr_update
            .first()
            .context("there should be at least one patch")?,
    ) else {
        git_repo.checkout(&branch_name)?;
        let _ = git_repo
            .apply_patch_chain(
                &branch_name,
                most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
            )
            .context("failed to apply patch chain")?;
        println!("checked out updated proposal as '{branch_name}' branch");
        return Ok(());
    };

    let Ok(proposal_tip) = str_to_sha1(&proposal_tip_str) else {
        git_repo.checkout(&branch_name)?;
        println!("checked out proposal as '{branch_name}' branch");
        return Ok(());
    };

    // Case 2: already up to date.
    if proposal_tip.eq(&local_branch_tip) {
        git_repo.checkout(&branch_name)?;
        println!("branch '{branch_name}' checked out and up-to-date");
        return Ok(());
    }

    // For cases 3-5 we need to know the ancestry relationship.
    if git_repo.does_commit_exist(&proposal_tip_str)? {
        let published_is_ancestor_of_local =
            git_repo.ancestor_of(&local_branch_tip, &proposal_tip)?;
        let local_is_ancestor_of_published =
            git_repo.ancestor_of(&proposal_tip, &local_branch_tip)?;

        // Case 3: branch is behind — local tip is an ancestor of the published
        // tip, meaning the author appended new patches. Fast-forward.
        if local_is_ancestor_of_published {
            git_repo.checkout(&branch_name)?;
            let _ = git_repo
                .apply_patch_chain(
                    &branch_name,
                    most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
                )
                .context("failed to apply patch chain")?;
            println!("checked out updated proposal as '{branch_name}' branch");
            return Ok(());
        }

        // Case 4: local has commits stacked on top of the published tip —
        // published tip is an ancestor of local tip. Check out without touching
        // commits.
        if published_is_ancestor_of_local {
            git_repo.checkout(&branch_name)?;
            println!(
                "checked out proposal branch '{branch_name}' (local branch has unpublished commits on top)"
            );
            return Ok(());
        }

        // Case 5: diverged — neither is an ancestor of the other.
        // This covers both local amendments and author force-pushes.
        // Require --force to overwrite.
        if force {
            git_repo.checkout(&branch_name)?;
            let _ = git_repo
                .apply_patch_chain(
                    &branch_name,
                    most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
                )
                .context("failed to apply patch chain")?;
            println!(
                "checked out updated proposal as '{branch_name}' branch (overwrote diverged local branch)"
            );
            return Ok(());
        }

        git_repo.checkout(&branch_name)?;
        print_diverged_branch_help(&branch_name);
        bail!(
            "branch '{branch_name}' has diverged from the published proposal; use --force to overwrite"
        );
    }

    // Published tip not found locally and branch already exists — the author
    // has published a new revision whose commits we don't have yet. Treat as
    // diverged: require --force to overwrite.
    if force {
        git_repo.checkout(&branch_name)?;
        let _ = git_repo
            .apply_patch_chain(
                &branch_name,
                most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
            )
            .context("failed to apply patch chain")?;
        println!(
            "checked out updated proposal as '{branch_name}' branch (overwrote diverged local branch)"
        );
        return Ok(());
    }

    git_repo.checkout(&branch_name)?;
    print_diverged_branch_help(&branch_name);
    bail!(
        "branch '{branch_name}' has diverged from the published proposal; use --force to overwrite"
    )
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

fn checkout_remote_branch_with_tracking(
    git_repo: &Repo,
    remote_name: &str,
    branch_name: &str,
) -> Result<()> {
    let remote_branch_ref = format!("refs/remotes/{remote_name}/{branch_name}");
    let remote_branch = git_repo
        .git_repo
        .find_reference(&remote_branch_ref)
        .context(format!("failed to find remote branch {remote_branch_ref}"))?;
    let commit = remote_branch
        .peel_to_commit()
        .context("failed to peel remote branch to commit")?;

    let mut local_branch = git_repo
        .git_repo
        .branch(branch_name, &commit, false)
        .context("failed to create local branch")?;

    local_branch
        .set_upstream(Some(&format!("{remote_name}/{branch_name}")))
        .context("failed to set upstream tracking")?;

    let local_branch_ref = local_branch.into_reference();
    let local_branch_ref_name = local_branch_ref
        .name()
        .context("failed to get local branch ref name")?;

    git_repo
        .git_repo
        .set_head(local_branch_ref_name)
        .context("failed to set head to local branch")?;
    git_repo
        .git_repo
        .checkout_head(None)
        .context("failed to checkout head")?;

    Ok(())
}
