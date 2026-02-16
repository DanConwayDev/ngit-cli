use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use ngit::{
    client::get_all_proposal_patch_pr_pr_update_events_from_cache,
    git_events::get_pr_tip_event_or_most_recent_patch_with_ancestors,
};
use nostr::nips::nip19::Nip19;
use nostr_sdk::{EventId, FromBech32};

use crate::{
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions},
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

pub async fn launch(id: &str, stdout: bool, offline: bool) -> Result<()> {
    let event_id = parse_event_id(id)?;

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(ngit::client::Params::with_git_config_relay_defaults(&Some(
        &git_repo,
    )));

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
        ngit::client::get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?;

    let proposal = proposals_and_revisions
        .iter()
        .find(|e| e.id == event_id)
        .context(format!(
            "proposal with id {} not found in cache",
            event_id.to_hex()
        ))?;

    let commits_events: Vec<nostr::Event> = get_all_proposal_patch_pr_pr_update_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal.id,
    )
    .await?;

    let patches = get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events.clone())
        .context("failed to find any PR or patch events on this proposal")?;

    if patches.iter().any(|e| {
        [
            ngit::git_events::KIND_PULL_REQUEST,
            ngit::git_events::KIND_PULL_REQUEST_UPDATE,
        ]
        .contains(&e.kind)
    }) {
        bail!(
            "this proposal uses PR format (not patches). Use `ngit checkout {}` instead.",
            event_id.to_hex()
        );
    }

    if stdout {
        output_patches_to_stdout(patches);
    } else {
        launch_git_am_with_patches(patches)?;
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

fn output_patches_to_stdout(mut patches: Vec<nostr::Event>) {
    patches.reverse();
    for patch in patches {
        print!("{}\n\n", patch.content);
    }
}

fn launch_git_am_with_patches(mut patches: Vec<nostr::Event>) -> Result<()> {
    println!("applying to current branch with `git am`");
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
