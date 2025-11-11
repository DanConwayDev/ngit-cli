use std::{collections::HashMap, path::PathBuf, str::FromStr};

use anyhow::{Result, anyhow};
use auth_git2::GitAuthenticator;
use nostr::hashes::sha1::Hash as Sha1Hash;

use crate::{
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
    },
    repo_ref::is_grasp_server_clone_url,
    utils::{Direction, get_read_protocols_to_try, join_with_and, set_protocol_preference},
};

pub fn list_from_remotes(
    term: &console::Term,
    git_repo: &Repo,
    git_servers: &Vec<String>,
    decoded_nostr_url: &NostrUrlDecoded,
) -> HashMap<String, (HashMap<String, String>, bool)> {
    let mut remote_states = HashMap::new();
    let mut errors = HashMap::new();
    for url in git_servers {
        let is_grasp_server = is_grasp_server_clone_url(url);
        match list_from_remote(term, git_repo, url, decoded_nostr_url, is_grasp_server) {
            Err(error) => {
                errors.insert(url, error);
            }
            Ok(state) => {
                remote_states.insert(url.to_string(), (state, is_grasp_server));
            }
        }
    }
    remote_states
}

pub fn list_from_remote(
    term: &console::Term,
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    is_grasp_server: bool,
) -> Result<HashMap<String, String>> {
    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_read_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

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

        let formatted_url = server_url.format_as(protocol)?;

        let res = list_from_remote_url(
            git_repo,
            &formatted_url,
            decoded_nostr_url.ssh_key_file_path().as_ref(),
            [ServerProtocol::UnauthHttps, ServerProtocol::UnauthHttp].contains(protocol),
            term,
        );

        match res {
            Ok(state) => {
                remote_state = Some(state);
                if !is_grasp_server && !failed_protocols.is_empty() {
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
                if is_grasp_server {
                    term.write_line(&format!("list: failed: {error}"))?;
                } else {
                    term.write_line(&format!(
                        "list: {formatted_url} failed over {protocol}{}: {error}",
                        if protocol == &ServerProtocol::Ssh {
                            if let Some(ssh_key_file) = &decoded_nostr_url.ssh_key_file_path() {
                                format!(" with ssh key from {ssh_key_file}")
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    ))?;
                }
                failed_protocols.push(protocol);
            }
        }
    }
    if let Some(remote_state) = remote_state {
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
        if !is_grasp_server {
            term.write_line(format!("list: {error}").as_str())?;
        }
        Err(error)
    }
}

fn list_from_remote_url(
    git_repo: &Repo,
    git_server_remote_url: &str,
    ssh_key_file: Option<&String>,
    dont_authenticate: bool,
    term: &console::Term,
) -> Result<HashMap<String, String>> {
    let git_config = git_repo.git_repo.config()?;

    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_remote_url)?;
    // authentication may be required
    let auth = {
        if dont_authenticate {
            GitAuthenticator::default()
        } else if git_server_remote_url.contains("git@") {
            if let Some(ssh_key_file) = ssh_key_file {
                GitAuthenticator::default()
                    .add_ssh_key_from_file(PathBuf::from_str(ssh_key_file)?, None)
            } else {
                GitAuthenticator::default()
            }
        } else {
            GitAuthenticator::default()
        }
    };
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
        // ignore dereferenced tags
        } else if !head.name().to_string().ends_with("^{}") {
            state.insert(head.name().to_string(), head.oid().to_string());
        }
    }
    git_server_remote.disconnect()?;
    Ok(state)
}

pub fn get_ahead_behind(
    git_repo: &Repo,
    base_ref_or_oid: &str,
    latest_ref_or_oid: &str,
) -> Result<(Vec<Sha1Hash>, Vec<Sha1Hash>)> {
    let base = git_repo.get_commit_or_tip_of_reference(base_ref_or_oid)?;
    let latest = git_repo.get_commit_or_tip_of_reference(latest_ref_or_oid)?;
    git_repo.get_commits_ahead_behind(&base, &latest)
}
