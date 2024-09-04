use core::str;
use std::collections::HashMap;

use anyhow::{Context, Result};
use auth_git2::GitAuthenticator;
use client::get_state_from_cache;
use git::RepoActions;
use git_events::{event_to_cover_letter, get_commit_id_from_patch};
use ngit::{client, git, git_events, login::get_curent_user, repo_ref};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;
use repo_ref::RepoRef;

use crate::{
    git::Repo,
    utils::{
        get_open_proposals, get_short_git_server_name, switch_clone_url_between_ssh_and_https,
    },
};

pub async fn run_list(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    for_push: bool,
) -> Result<HashMap<String, HashMap<String, String>>> {
    let nostr_state =
        if let Ok(nostr_state) = get_state_from_cache(git_repo.get_path()?, repo_ref).await {
            Some(nostr_state)
        } else {
            None
        };

    let term = console::Term::stderr();

    let remote_states = list_from_remotes(&term, git_repo, &repo_ref.git_server)?;

    let mut state = if let Some(nostr_state) = nostr_state {
        for (name, value) in &nostr_state.state {
            for (url, remote_state) in &remote_states {
                let remote_name = get_short_git_server_name(git_repo, url);
                if let Some(remote_value) = remote_state.get(name) {
                    if value.ne(remote_value) {
                        term.write_line(
                            format!(
                                "WARNING: {remote_name} {name} is {} nostr ",
                                if let Ok((ahead, behind)) =
                                    get_ahead_behind(git_repo, value, remote_value)
                                {
                                    format!("{} ahead {} behind", ahead.len(), behind.len())
                                } else {
                                    "out of sync with".to_string()
                                }
                            )
                            .as_str(),
                        )?;
                    }
                } else {
                    term.write_line(
                        format!("WARNING: {remote_name} {name} is missing but tracked on nostr")
                            .as_str(),
                    )?;
                }
            }
        }
        nostr_state.state
    } else {
        repo_ref
            .git_server
            .iter()
            .filter_map(|server| remote_states.get(server))
            .cloned()
            .collect::<Vec<HashMap<String, String>>>()
            .first()
            .context("failed to get refs from git server")?
            .clone()
    };

    state.retain(|k, _| !k.starts_with("refs/heads/pr/"));

    let open_proposals = get_open_proposals(git_repo, repo_ref).await?;
    let current_user = get_curent_user(git_repo)?;
    for (_, (proposal, patches)) in open_proposals {
        if let Ok(cl) = event_to_cover_letter(&proposal) {
            if let Ok(mut branch_name) = cl.get_branch_name() {
                branch_name = if let Some(public_key) = current_user {
                    if proposal.author().eq(&public_key) {
                        cl.branch_name.to_string()
                    } else {
                        branch_name
                    }
                } else {
                    branch_name
                };
                if let Some(patch) = patches.first() {
                    // TODO this isn't resilient because the commit id stated may not be correct
                    // we will need to check whether the commit id exists in the repo or apply the
                    // proposal and each patch to check
                    if let Ok(commit_id) = get_commit_id_from_patch(patch) {
                        state.insert(format!("refs/heads/{branch_name}"), commit_id);
                    }
                }
            }
        }
    }

    // TODO 'for push' should we check with the git servers to see if any of them
    // allow push from the user?
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
    Ok(remote_states)
}

pub fn list_from_remotes(
    term: &console::Term,
    git_repo: &Repo,
    git_servers: &Vec<String>,
) -> Result<HashMap<String, HashMap<String, String>>> {
    let mut remote_states = HashMap::new();
    for url in git_servers {
        let short_name = get_short_git_server_name(git_repo, url);
        term.write_line(format!("fetching refs list: {short_name}...").as_str())?;
        match list_from_remote(git_repo, url) {
            Ok(remote_state) => {
                remote_states.insert(url.clone(), remote_state);
            }
            Err(error1) => {
                if let Ok(alternative_url) = switch_clone_url_between_ssh_and_https(url) {
                    match list_from_remote(git_repo, &alternative_url) {
                        Ok(remote_state) => {
                            remote_states.insert(url.clone(), remote_state);
                        }
                        Err(error2) => {
                            term.write_line(
                                format!("WARNING: {short_name} failed to list refs error: {error1}\r\nand alternative protocol {alternative_url}: {error2}").as_str(),
                            )?;
                        }
                    }
                } else {
                    term.write_line(
                        format!("WARNING: {short_name} failed to list refs error: {error1}",)
                            .as_str(),
                    )?;
                }
            }
        }
        term.clear_last_lines(1)?;
    }
    Ok(remote_states)
}

fn list_from_remote(
    git_repo: &Repo,
    git_server_remote_url: &str,
) -> Result<HashMap<String, String>> {
    let git_config = git_repo.git_repo.config()?;

    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_remote_url)?;
    // authentication may be required
    let auth = GitAuthenticator::default();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    remote_callbacks.credentials(auth.credentials(&git_config));
    git_server_remote.connect_auth(git2::Direction::Fetch, Some(remote_callbacks), None)?;
    let mut state = HashMap::new();
    for head in git_server_remote.list()? {
        if let Some(symbolic_reference) = head.symref_target() {
            state.insert(
                head.name().to_string(),
                format!("ref: {symbolic_reference}"),
            );
        } else {
            state.insert(head.name().to_string(), head.oid().to_string());
        }
    }
    git_server_remote.disconnect()?;
    Ok(state)
}

fn get_ahead_behind(
    git_repo: &Repo,
    base_ref_or_oid: &str,
    latest_ref_or_oid: &str,
) -> Result<(Vec<Sha1Hash>, Vec<Sha1Hash>)> {
    let base = git_repo.get_commit_or_tip_of_reference(base_ref_or_oid)?;
    let latest = git_repo.get_commit_or_tip_of_reference(latest_ref_or_oid)?;
    git_repo.get_commits_ahead_behind(&base, &latest)
}
