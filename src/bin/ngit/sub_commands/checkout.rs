use std::{
    collections::HashSet,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
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
    git_events::{event_to_cover_letter, patch_supports_commit_ids},
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

pub async fn launch(id: &str) -> Result<()> {
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

    if let Some((remote_name, _)) = &nostr_remote {
        run_git_fetch(remote_name)?;
    } else {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal = proposals_and_revisions
        .iter()
        .find(|e| e.id == event_id)
        .context(format!("proposal with id {} not found in cache", event_id.to_hex()))?;

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
        )
    } else {
        checkout_patch(
            &git_repo,
            &cover_letter,
            &most_recent_proposal_patch_chain_or_pr_or_pr_update,
            nostr_remote.as_ref().map(|(name, _)| name.as_str()),
        )
    }
}

fn run_git_fetch(remote_name: &str) -> Result<()> {
    println!("fetching from {remote_name}...");
    let exit_status = Command::new("git")
        .args(["fetch", remote_name])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git fetch")?;

    if !exit_status.success() {
        bail!("git fetch {remote_name} exited with error: {exit_status}");
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

fn checkout_pr(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    cover_letter: &crate::git_events::CoverLetter,
    most_recent_proposal_patch_chain_or_pr_or_pr_update: &[nostr::Event],
    nostr_remote_name: Option<&str>,
) -> Result<()> {
    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
    let proposal_tip_event = most_recent_proposal_patch_chain_or_pr_or_pr_update
        .first()
        .context("most_recent_proposal_patch_chain_or_pr_or_pr_update will always contain an event with c tag")?;
    let proposal_tip = tag_value(proposal_tip_event, "c")?;

    if let Ok(local_branch_tip) = git_repo.get_tip_of_branch(&branch_name) {
        git_repo
            .checkout(&branch_name)
            .context("cannot checkout existing proposal branch")?;
        if local_branch_tip.to_string() == proposal_tip {
            println!("checked out up-to-date proposal branch '{branch_name}'");
            return Ok(());
        }
        if git_repo.does_commit_exist(&proposal_tip)? {
            git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
            git_repo.checkout(&branch_name)?;
            println!("checked out proposal branch and updated tip '{branch_name}'");
            return Ok(());
        }
    }

    if let Some(remote_name) = nostr_remote_name {
        let remote_branch = format!("{remote_name}/{branch_name}");
        if git_repo.get_tip_of_branch(&remote_branch).is_ok() {
            checkout_remote_branch_with_tracking(git_repo, remote_name, &branch_name)?;
            println!("checked out proposal branch '{branch_name}' with tracking to {remote_name}");
            return Ok(());
        }
    }

    fetch_oid_for_from_servers_for_pr(
        &proposal_tip,
        git_repo,
        repo_ref,
        proposal_tip_event,
    )?;
    git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
    git_repo.checkout(&branch_name)?;
    println!("created and checked out proposal branch '{branch_name}'");
    Ok(())
}

fn checkout_patch(
    git_repo: &Repo,
    cover_letter: &crate::git_events::CoverLetter,
    most_recent_proposal_patch_chain_or_pr_or_pr_update: &[nostr::Event],
    nostr_remote_name: Option<&str>,
) -> Result<()> {
    let no_support_for_patches_as_branch = most_recent_proposal_patch_chain_or_pr_or_pr_update
        .iter()
        .any(|event| !patch_supports_commit_ids(event));

    if no_support_for_patches_as_branch {
        bail!(
            "this proposal cannot be checked out as a branch because some patches do not have a parent commit.\n\
             Try `ngit apply --stdout` to apply patches to the current branch, or use `ngit list` for interactive options."
        );
    }

    let proposal_base_commit = str_to_sha1(&tag_value(
        most_recent_proposal_patch_chain_or_pr_or_pr_update
            .last()
            .context("there should be at least one patch")?,
        "parent-commit",
    )?)
    .context("failed to get valid parent commit id from patch")?;

    let (main_branch_name, _master_tip) = git_repo.get_main_or_master_branch()?;

    if !git_repo.does_commit_exist(&proposal_base_commit.to_string())? {
        bail!(
            "the proposal parent commit doesn't exist in your local repository.\n\
             Try running `git pull` on '{main_branch_name}' first, or use `ngit apply --stdout` to apply patches to the current branch."
        );
    }

    if git_repo.has_outstanding_changes()? {
        bail!(
            "working directory is not clean. Discard or stash (un)staged changes and try again."
        );
    }

    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
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
                println!("checked out proposal branch '{branch_name}' with tracking to {remote_name}");
                return Ok(());
            }
        }
        let _ = git_repo
            .apply_patch_chain(&branch_name, most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec())
            .context("failed to apply patch chain")?;
        println!("checked out proposal as '{branch_name}' branch");
        return Ok(());
    }

    let local_branch_tip = git_repo.get_tip_of_branch(&branch_name)?;

    let proposal_tip = str_to_sha1(
        &get_commit_id_from_patch(
            most_recent_proposal_patch_chain_or_pr_or_pr_update
                .first()
                .context("there should be at least one patch")?,
        )
        .context("failed to get valid commit_id from patch")?,
    )
    .context("failed to get valid commit_id from patch")?;

    if proposal_tip.eq(&local_branch_tip) {
        git_repo.checkout(&branch_name)?;
        println!("branch '{branch_name}' checked out and up-to-date");
        return Ok(());
    }

    git_repo.create_branch_at_commit(&branch_name, &proposal_base_commit.to_string())?;
    git_repo.checkout(&branch_name)?;
    let _ = git_repo
        .apply_patch_chain(&branch_name, most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec())
        .context("failed to apply patch chain")?;
    println!("checked out updated proposal as '{branch_name}' branch");
    Ok(())
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
