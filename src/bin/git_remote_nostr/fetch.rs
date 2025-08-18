use core::str;
use std::{
    collections::{HashMap, HashSet},
    io::Stdin,
};

use anyhow::{Context, Result, bail};
use ngit::{
    fetch::fetch_from_git_server,
    git::{Repo, RepoActions},
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        identify_clone_urls_for_oids_from_pr_pr_update_events, tag_value,
    },
    login::get_curent_user,
    repo_ref::{RepoRef, is_grasp_server_in_list},
    utils::{
        find_proposal_and_patches_by_branch_name, get_oids_from_fetch_batch,
        get_open_or_draft_proposals,
    },
};
use nostr::nips::nip19;
use nostr_sdk::{Event, ToBech32};

pub async fn run_fetch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    stdin: &Stdin,
    oid: &str,
    refstr: &str,
) -> Result<()> {
    let mut fetch_batch = get_oids_from_fetch_batch(stdin, oid, refstr)?;

    let oids_from_state = fetch_batch
        .iter()
        .filter(|(refstr, _)| !refstr.contains("refs/heads/pr/"))
        .map(|(_, oid)| oid.clone())
        .collect::<Vec<String>>();

    let pr_oid_clone_url_map = identify_clone_urls_for_oids_from_pr_pr_update_events(
        fetch_batch.values().collect::<Vec<&String>>(),
        git_repo,
        repo_ref,
    )
    .await?;

    let oids_to_fetch_from_git_servers = [
        oids_from_state.clone(),
        pr_oid_clone_url_map
            .keys()
            .cloned()
            .collect::<Vec<String>>(),
    ]
    .concat();

    let git_servers = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = vec![];
        for server in &repo_ref.git_server {
            if seen.insert(server.clone()) {
                out.push(server.clone());
            }
        }
        for url in pr_oid_clone_url_map.values().flatten() {
            if seen.insert(url.clone()) {
                out.push(url.clone());
            }
        }
        out
    };

    let mut errors = vec![];
    let term = console::Term::stderr();

    for git_server_url in &git_servers {
        let oids_to_fetch_from_server = oids_to_fetch_from_git_servers
            .clone()
            .into_iter()
            .filter(|oid| !git_repo.does_commit_exist(oid).unwrap_or(false))
            .collect::<Vec<String>>();

        if oids_to_fetch_from_server.is_empty() {
            continue;
        }

        let term = console::Term::stderr();
        if let Err(error) = fetch_from_git_server(
            git_repo,
            &oids_from_state,
            git_server_url,
            &repo_ref.to_nostr_git_url(&None),
            &term,
            is_grasp_server_in_list(git_server_url, &repo_ref.grasp_servers()),
        ) {
            errors.push(error);
        }
    }

    if oids_from_state
        .iter()
        .any(|oid| !git_repo.does_commit_exist(oid).unwrap())
        && !errors.is_empty()
    {
        bail!(
            "fetch: failed to fetch objects from:\r\n{}",
            errors
                .iter()
                .map(|e| format!(" - {e}"))
                .collect::<Vec<String>>()
                .join("\r\n")
        );
    }

    fetch_batch.retain(|refstr, _| refstr.contains("refs/heads/pr/"));

    fetch_open_or_draft_proposals_from_patches(git_repo, &term, repo_ref, &fetch_batch).await?;
    // TODO fetch_open_or_draft_proposals just needs to do it for patches
    term.flush()?;
    println!();
    Ok(())
}

pub fn make_commits_for_proposal(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    patches_ancestor_last: &[Event],
) -> Result<String> {
    let patches_ancestor_first: Vec<&Event> = patches_ancestor_last.iter().rev().collect();
    let mut tip_commit_id = if let Ok(parent_commit) = tag_value(
        patches_ancestor_first
            .first()
            .context("proposal should have at least one patch")?,
        "parent-commit",
    ) {
        parent_commit
    } else {
        // TODO choose most recent commit on master before patch timestamp so it doesnt
        // constantly get rebased
        let (_, hash) = git_repo.get_main_or_master_branch()?;
        hash.to_string()
    };

    for patch in &patches_ancestor_first {
        let commit_id = git_repo
            .create_commit_from_patch(patch, Some(tip_commit_id.clone()))
            .context(format!(
                "failed to create commit for patch {}",
                nip19::Nip19Event {
                    event_id: patch.id,
                    author: Some(patch.pubkey),
                    kind: Some(patch.kind),
                    relays: if let Some(relay) = repo_ref.relays.first() {
                        vec![relay.to_owned()]
                    } else {
                        vec![]
                    },
                }
                .to_bech32()
                .unwrap_or_default()
            ))?;
        tip_commit_id = commit_id.to_string();
    }
    Ok(tip_commit_id)
}

async fn fetch_open_or_draft_proposals_from_patches(
    git_repo: &Repo,
    term: &console::Term,
    repo_ref: &RepoRef,
    proposal_refs: &HashMap<String, String>,
) -> Result<()> {
    if !proposal_refs.is_empty() {
        let open_and_draft_proposals = get_open_or_draft_proposals(git_repo, repo_ref).await?;

        let current_user = get_curent_user(git_repo)?;

        for refstr in proposal_refs.keys() {
            if let Some((_, (_, events_to_apply))) = find_proposal_and_patches_by_branch_name(
                refstr,
                &open_and_draft_proposals,
                current_user.as_ref(),
            ) {
                if events_to_apply
                    .iter()
                    .any(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
                {
                    // do nothing - we fetch these oids as part of run_fetch
                } else if let Err(error) =
                    make_commits_for_proposal(git_repo, repo_ref, events_to_apply)
                {
                    term.write_line(
                        format!("WARNING: failed to create branch for {refstr}, error: {error}",)
                            .as_str(),
                    )?;
                    break;
                }
            }
        }
    }
    Ok(())
}
