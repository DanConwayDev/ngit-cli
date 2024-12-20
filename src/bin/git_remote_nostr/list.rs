use core::str;
use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use auth_git2::GitAuthenticator;
use client::get_state_from_cache;
use git::RepoActions;
use ngit::{
    client,
    git::{
        self,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
    },
    git_events::event_to_cover_letter,
    login::get_curent_user,
    repo_ref,
};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;
use repo_ref::RepoRef;

use crate::{
    fetch::{fetch_from_git_server, make_commits_for_proposal},
    git::Repo,
    utils::{
        Direction, fetch_or_list_error_is_not_authentication_failure, get_open_or_draft_proposals,
        get_read_protocols_to_try, get_short_git_server_name, join_with_and,
        set_protocol_preference,
    },
};

pub async fn run_list(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    for_push: bool,
) -> Result<HashMap<String, HashMap<String, String>>> {
    let nostr_state =
        if let Ok(nostr_state) = get_state_from_cache(Some(git_repo.get_path()?), repo_ref).await {
            Some(nostr_state)
        } else {
            None
        };

    let term = console::Term::stderr();

    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        &repo_ref.to_nostr_git_url(&None),
    );

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

    let proposals_state =
        get_open_and_draft_proposals_state(&term, git_repo, repo_ref, &remote_states).await?;

    state.extend(proposals_state);

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

async fn get_open_and_draft_proposals_state(
    term: &console::Term,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    remote_states: &HashMap<String, HashMap<String, String>>,
) -> Result<HashMap<String, String>> {
    // we cannot use commit_id in the latest patch in a proposal because:
    // 1) the `commit` tag is optional
    // 2) if the commit tag is wrong, it will cause errors which stop clone from
    //    working

    // without trusting commit_id we must apply each patch which requires the oid of
    // the parent so we much do a fetch
    for (git_server_url, oids_from_git_servers) in remote_states {
        if fetch_from_git_server(
            git_repo,
            &oids_from_git_servers
                .values()
                .filter(|v| !v.starts_with("ref: "))
                .cloned()
                .collect::<Vec<String>>(),
            git_server_url,
            &repo_ref.to_nostr_git_url(&None),
            term,
        )
        .is_ok()
        {
            break;
        }
    }

    let mut state = HashMap::new();
    let open_and_draft_proposals = get_open_or_draft_proposals(git_repo, repo_ref).await?;
    let current_user = get_curent_user(git_repo)?;
    for (_, (proposal, patches)) in open_and_draft_proposals {
        if let Ok(cl) = event_to_cover_letter(&proposal) {
            if let Ok(mut branch_name) = cl.get_branch_name_with_pr_prefix_and_shorthand_id() {
                branch_name = if let Some(public_key) = current_user {
                    if proposal.pubkey.eq(&public_key) {
                        format!("pr/{}", cl.branch_name_without_id_or_prefix)
                    } else {
                        branch_name
                    }
                } else {
                    branch_name
                };
                match make_commits_for_proposal(git_repo, repo_ref, &patches) {
                    Ok(tip) => {
                        state.insert(format!("refs/heads/{branch_name}"), tip);
                    }
                    Err(error) => {
                        let _ = term.write_line(
                            format!("WARNING: failed to fetch branch {branch_name} error: {error}")
                                .as_str(),
                        );
                    }
                };
            }
        }
    }
    Ok(state)
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
