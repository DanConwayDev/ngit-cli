use std::io::Write;

use anyhow::{Context, Result, bail};
use ngit::{
    client::get_all_proposal_patch_pr_pr_update_events_from_cache,
    fetch::ensure_commit_local,
    git::str_to_sha1,
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, pr_event_clone_tag_urls, tag_value,
    },
    git_http_auth::prepare_private_git_auth_for_repo,
    repo_ref::RepoRef,
};

use crate::{
    cli::SignerParams,
    client::{Client, Connect, get_repo_ref_from_cache, warn_if_invited_as_maintainer},
    git::{Repo, RepoActions},
    repo_ref::get_repo_coordinates_when_remote_unknown,
    sub_commands::{
        id_resolver::{pr_description, resolve_pr_root_or_prefix},
        repository_fetch::fetching_with_account,
    },
};

pub async fn launch(id: &str, stdout: bool, offline: bool, auth: SignerParams<'_>) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(ngit::client::Params::with_git_config_relay_defaults(&Some(
        &git_repo,
    )));

    let mut repo_coordinates =
        get_repo_coordinates_when_remote_unknown(&git_repo, &mut client).await?;

    if !offline {
        fetching_with_account(
            &git_repo,
            git_repo_path,
            &mut client,
            &mut repo_coordinates,
            auth,
        )
        .await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;
    let private_signer =
        prepare_private_git_auth_for_repo(&repo_ref, &git_repo, auth.info, auth.password).await?;

    let proposals_and_revisions: Vec<nostr::prelude::Event> =
        ngit::client::get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?;

    let proposal = resolve_pr_root_or_prefix(id, proposals_and_revisions.iter(), pr_description)?;

    let commits_events: Vec<nostr::prelude::Event> =
        get_all_proposal_patch_pr_pr_update_events_from_cache(
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
        apply_pr(
            &git_repo,
            &repo_ref,
            pr_event,
            stdout,
            private_signer.as_ref(),
        )
        .await?;
        return Ok(());
    }

    if stdout {
        output_patches_to_stdout(patches);
    } else {
        launch_git_am_with_patches(patches)?;
    }

    Ok(())
}

async fn apply_pr(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    pr_event: &nostr::prelude::Event,
    stdout: bool,
    private_signer: Option<&std::sync::Arc<ngit::signer::NgitSigner>>,
) -> Result<()> {
    let tip_oid = tag_value(pr_event, "c").context("PR event is missing 'c' (tip commit) tag")?;

    // Ensure the tip commit is available locally. `ensure_commit_local`
    // short-circuits if the commit is already present.
    let extras = pr_event_clone_tag_urls(pr_event);
    ensure_commit_local(
        &tip_oid,
        git_repo,
        repo_ref,
        &extras,
        &console::Term::stderr(),
        private_signer,
    )
    .await?;

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
        if crate::output::is_json() {
            crate::output::set_value(serde_json::json!({
                "status": "ok",
                "patches": patch_texts,
            }));
        } else {
            for patch in &patch_texts {
                print!("{patch}\n\n");
            }
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
        .stdout(if crate::output::is_json() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::inherit()
        })
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
    let output = am
        .wait_with_output()
        .context("failed to read git am output")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !output.status.success() {
        bail!("git am failed");
    }
    Ok(())
}

fn output_patches_to_stdout(mut patches: Vec<nostr::prelude::Event>) {
    patches.reverse();
    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "patches": patches.into_iter().map(|patch| patch.content).collect::<Vec<_>>(),
        }));
    } else {
        for patch in patches {
            print!("{}\n\n", patch.content);
        }
    }
}

fn launch_git_am_with_patches(mut patches: Vec<nostr::prelude::Event>) -> Result<()> {
    patches.reverse();
    apply_patch_texts(patches.into_iter().map(|p| p.content).collect())
}
