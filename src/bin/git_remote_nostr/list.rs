use core::str;
use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use auth_git2::GitAuthenticator;
use client::get_state_from_cache;
use git::RepoActions;
use git_events::{event_to_cover_letter, get_commit_id_from_patch};
use ngit::{
    client,
    git::{
        self,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
    },
    git_events,
    login::get_curent_user,
    repo_ref,
};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;
use repo_ref::RepoRef;

use crate::{
    git::Repo,
    utils::{
        fetch_or_list_error_is_not_authentication_failure, get_open_proposals,
        get_read_protocols_to_try, get_short_git_server_name, join_with_and,
        set_protocol_preference, Direction,
    },
};

pub async fn run_list(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    decoded_nostr_url: &NostrUrlDecoded,
    for_push: bool,
) -> Result<HashMap<String, HashMap<String, String>>> {
    let nostr_state =
        if let Ok(nostr_state) = get_state_from_cache(git_repo.get_path()?, repo_ref).await {
            Some(nostr_state)
        } else {
            None
        };

    let term = console::Term::stderr();

    let remote_states = list_from_remotes(&term, git_repo, &repo_ref.git_server, decoded_nostr_url);

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
    decoded_nostr_url: &NostrUrlDecoded, // Add this parameter
) -> HashMap<String, HashMap<String, String>> {
    let mut remote_states = HashMap::new();
    let mut errors = HashMap::new();
    for url in git_servers {
        match list_from_remote(term, git_repo, url, decoded_nostr_url) {
            Err(error) => {
                errors.insert(url, error);
            }
            Ok(state) => {
                remote_states.insert(url.to_string(), state);
            }
        }
    }
    remote_states
}

pub fn list_from_remote(
    term: &console::Term,
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded, // Add this parameter
) -> Result<HashMap<String, String>> {
    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt = get_read_protocols_to_try(git_repo, &server_url, decoded_nostr_url);

    let mut failed_protocols = vec![];
    let mut remote_state: Option<HashMap<String, String>> = None;

    for protocol in &protocols_to_attempt {
        term.write_line(
            format!(
                "fetching {} ref list over {protocol}...",
                server_url.short_name(),
            )
            .as_str(),
        )?;

        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;
        let res = list_from_remote_url(
            git_repo,
            &formatted_url,
            [ServerProtocol::UnauthHttps, ServerProtocol::UnauthHttp].contains(protocol),
            term,
        );

        match res {
            Ok(state) => {
                remote_state = Some(state);
                term.clear_last_lines(1)?;
                if !failed_protocols.is_empty() {
                    term.write_line(
                        format!(
                            "list: succeeded over {protocol} from {}",
                            server_url.short_name(),
                        )
                        .as_str(),
                    )?;
                    let _ =
                        set_protocol_preference(git_repo, protocol, &server_url, &Direction::Fetch);
                }
                break;
            }
            Err(error) => {
                term.clear_last_lines(1)?;
                term.write_line(
                    format!("list: {formatted_url} failed over {protocol}: {error}").as_str(),
                )?;
                failed_protocols.push(protocol);
                if protocol == &ServerProtocol::Ssh
                    && fetch_or_list_error_is_not_authentication_failure(&error)
                {
                    // authenticated by failed to complete request
                    break;
                }
            }
        }
    }
    if let Some(remote_state) = remote_state {
        if failed_protocols.is_empty() {
            term.clear_last_lines(1)?;
        }
        Ok(remote_state)
    } else {
        let error = anyhow!(
            "{} failed over {}{}",
            server_url.short_name(),
            join_with_and(&failed_protocols),
            if decoded_nostr_url.protocol.is_some() {
                " and nostr url contains protocol override so no other protocols were attempted"
            } else {
                ""
            },
        );
        term.write_line(format!("list: {error}").as_str())?;
        Err(error)
    }
}

fn list_from_remote_url(
    git_repo: &Repo,
    git_server_remote_url: &str,
    dont_authenticate: bool,
    term: &console::Term,
) -> Result<HashMap<String, String>> {
    let git_config = git_repo.git_repo.config()?;

    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_remote_url)?;
    // authentication may be required
    let auth = GitAuthenticator::default();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    if !dont_authenticate {
        remote_callbacks.credentials(auth.credentials(&git_config));
    }
    term.write_line("list: connecting...")?;
    git_server_remote.connect_auth(git2::Direction::Fetch, Some(remote_callbacks), None)?;
    term.clear_last_lines(1)?;
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
