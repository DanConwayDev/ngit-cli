use std::{
    collections::HashSet,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use ngit::{
    client::get_all_proposal_patch_pr_pr_update_events_from_cache,
    fetch::fetch_from_git_server,
    git::str_to_sha1,
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, tag_value,
    },
    repo_ref::{RepoRef, is_grasp_server_in_list},
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

    if patches
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind))
    {
        let pr_event = patches
            .first()
            .context("patch chain should contain at least one event")?;
        apply_pr(&git_repo, &repo_ref, pr_event, stdout)?;
        return Ok(());
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

fn fetch_oid_for_pr(
    oid: &str,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    pr_event: &nostr::Event,
) -> Result<()> {
    let git_servers = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = vec![];
        for tag in pr_event.tags.as_slice() {
            if tag.kind().eq(&nostr::event::TagKind::Clone) {
                for clone_url in tag.as_slice().iter().skip(1) {
                    seen.insert(clone_url.clone());
                    out.push(clone_url.clone());
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

    let term = console::Term::stderr();
    for git_server_url in &git_servers {
        if fetch_from_git_server(
            git_repo,
            &[oid.to_string()],
            git_server_url,
            &repo_ref.to_nostr_git_url(&None),
            &term,
            is_grasp_server_in_list(git_server_url, &repo_ref.grasp_servers()),
        )
        .is_ok()
        {
            return Ok(());
        }
    }
    if !git_repo.does_commit_exist(oid)? {
        bail!(
            "cannot find proposal git data from proposal git server hint or repository git servers"
        );
    }
    Ok(())
}

fn apply_pr(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    pr_event: &nostr::Event,
    stdout: bool,
) -> Result<()> {
    let tip_oid = tag_value(pr_event, "c").context("PR event is missing 'c' (tip commit) tag")?;

    // Ensure the tip commit is available locally
    if !git_repo.does_commit_exist(&tip_oid)? {
        fetch_oid_for_pr(&tip_oid, git_repo, repo_ref, pr_event)?;
    }

    let tip = str_to_sha1(&tip_oid).context("invalid tip commit OID in PR event")?;

    // Determine the base commit: prefer the merge-base tag, fall back to
    // computing the divergence point from main/master.
    let base = if let Ok(merge_base_oid) = tag_value(pr_event, "merge-base") {
        str_to_sha1(&merge_base_oid).context("invalid merge-base OID in PR event")?
    } else {
        let (_, main_tip) = git_repo
            .get_main_or_master_branch()
            .context("could not determine main branch to compute PR base commit")?;
        let (ahead, _behind) = git_repo
            .get_commits_ahead_behind(&main_tip, &tip)
            .context("failed to compute commits between main and PR tip")?;
        // ahead is youngest-first; the last element is the oldest PR commit,
        // whose parent is the effective base.
        let oldest_pr_commit = ahead
            .last()
            .context("no commits found between main and PR tip")?;
        git_repo
            .get_commit_parent(oldest_pr_commit)
            .context("failed to get parent of the oldest PR commit")?
    };

    // Collect commits from base..tip (youngest-first from get_commits_ahead_behind)
    let (commits_youngest_first, _) = git_repo
        .get_commits_ahead_behind(&base, &tip)
        .context("failed to enumerate commits in PR")?;

    if commits_youngest_first.is_empty() {
        bail!("no commits found between base and PR tip");
    }

    let total = commits_youngest_first.len() as u64;

    // Generate patches oldest-first
    let mut patch_texts: Vec<String> = Vec::with_capacity(commits_youngest_first.len());
    for (i, commit) in commits_youngest_first.iter().rev().enumerate() {
        let series_count = Some((i as u64 + 1, total));
        let patch = git_repo
            .make_patch_from_commit(commit, &series_count)
            .with_context(|| format!("failed to generate patch for commit {commit}"))?;
        patch_texts.push(patch);
    }

    if stdout {
        for patch in &patch_texts {
            print!("{patch}\n\n");
        }
    } else {
        apply_patch_texts(patch_texts)?;
    }

    Ok(())
}

fn apply_patch_texts(patch_texts: Vec<String>) -> Result<()> {
    println!("applying to current branch with `git am`");

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

    for patch in patch_texts {
        stdin
            .write(format!("{patch}\n\n").as_bytes())
            .context("failed to write patch content into git am stdin buffer")?;
    }
    stdin.flush()?;
    am.wait_with_output()
        .context("failed to read git am stdout")?;
    Ok(())
}

fn output_patches_to_stdout(mut patches: Vec<nostr::Event>) {
    patches.reverse();
    for patch in patches {
        print!("{}\n\n", patch.content);
    }
}

fn launch_git_am_with_patches(mut patches: Vec<nostr::Event>) -> Result<()> {
    patches.reverse();
    apply_patch_texts(patches.into_iter().map(|p| p.content).collect())
}
