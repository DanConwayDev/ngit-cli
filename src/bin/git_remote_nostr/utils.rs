use core::str;
use std::{
    collections::HashMap,
    fmt,
    io::{self, Stdin},
    str::FromStr,
};

use anyhow::{bail, Context, Result};
use git2::Repository;
use ngit::{
    client::{
        get_all_proposal_patch_events_from_cache, get_events_from_cache,
        get_proposals_and_revisions_from_cache,
    },
    git::{
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        Repo, RepoActions,
    },
    git_events::{
        event_is_revision_root, get_most_recent_patch_with_ancestors,
        is_event_proposal_root_for_branch, status_kinds,
    },
    repo_ref::RepoRef,
};
use nostr_sdk::{Event, EventId, Kind, PublicKey, Url};

pub fn get_short_git_server_name(git_repo: &Repo, url: &str) -> std::string::String {
    if let Ok(name) = get_remote_name_by_url(&git_repo.git_repo, url) {
        return name;
    }
    if let Ok(url) = Url::parse(url) {
        if let Some(domain) = url.domain() {
            return domain.to_string();
        }
    }
    url.to_string()
}

pub fn get_remote_name_by_url(git_repo: &Repository, url: &str) -> Result<String> {
    let remotes = git_repo.remotes()?;
    Ok(remotes
        .iter()
        .find(|r| {
            if let Some(name) = r {
                if let Some(remote_url) = git_repo.find_remote(name).unwrap().url() {
                    url == remote_url
                } else {
                    false
                }
            } else {
                false
            }
        })
        .context("could not find remote with matching url")?
        .context("remote with matching url must be named")?
        .to_string())
}

pub fn get_oids_from_fetch_batch(
    stdin: &Stdin,
    initial_oid: &str,
    initial_refstr: &str,
) -> Result<HashMap<String, String>> {
    let mut line = String::new();
    let mut batch = HashMap::new();
    batch.insert(initial_refstr.to_string(), initial_oid.to_string());
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["fetch", oid, refstr] => {
                batch.insert((*refstr).to_string(), (*oid).to_string());
            }
            [] => break,
            _ => bail!(
                "after a `fetch` command we are only expecting another fetch or an empty line"
            ),
        }
    }
    Ok(batch)
}

/// Read one line from stdin, and split it into tokens.
pub fn read_line<'a>(stdin: &io::Stdin, line: &'a mut String) -> io::Result<Vec<&'a str>> {
    line.clear();

    let read = stdin.read_line(line)?;
    if read == 0 {
        return Ok(vec![]);
    }
    let line = line.trim();
    let tokens = line.split(' ').filter(|t| !t.is_empty()).collect();

    Ok(tokens)
}

pub async fn get_open_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<EventId, (Event, Vec<Event>)>> {
    let git_repo_path = git_repo.get_path()?;
    let proposals: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .filter(|e| !event_is_revision_root(e))
            .cloned()
            .collect();

    let statuses: Vec<nostr::Event> = {
        let mut statuses = get_events_from_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals.iter().map(nostr::Event::id)),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };
    let mut open_proposals = HashMap::new();

    for proposal in proposals {
        let status = if let Some(e) = statuses
            .iter()
            .filter(|e| {
                status_kinds().contains(&e.kind())
                    && e.tags()
                        .iter()
                        .any(|t| t.as_vec()[1].eq(&proposal.id.to_string()))
            })
            .collect::<Vec<&nostr::Event>>()
            .first()
        {
            e.kind()
        } else {
            Kind::GitStatusOpen
        };
        if status.eq(&Kind::GitStatusOpen) {
            if let Ok(commits_events) =
                get_all_proposal_patch_events_from_cache(git_repo_path, repo_ref, &proposal.id)
                    .await
            {
                if let Ok(most_recent_proposal_patch_chain) =
                    get_most_recent_patch_with_ancestors(commits_events.clone())
                {
                    open_proposals
                        .insert(proposal.id(), (proposal, most_recent_proposal_patch_chain));
                }
            }
        }
    }
    Ok(open_proposals)
}

pub async fn get_all_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<EventId, (Event, Vec<Event>)>> {
    let git_repo_path = git_repo.get_path()?;
    let proposals: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .filter(|e| !event_is_revision_root(e))
            .cloned()
            .collect();

    let mut all_proposals = HashMap::new();

    for proposal in proposals {
        if let Ok(commits_events) =
            get_all_proposal_patch_events_from_cache(git_repo_path, repo_ref, &proposal.id).await
        {
            if let Ok(most_recent_proposal_patch_chain) =
                get_most_recent_patch_with_ancestors(commits_events.clone())
            {
                all_proposals.insert(proposal.id(), (proposal, most_recent_proposal_patch_chain));
            }
        }
    }
    Ok(all_proposals)
}

pub fn find_proposal_and_patches_by_branch_name<'a>(
    refstr: &'a str,
    open_proposals: &'a HashMap<EventId, (Event, Vec<Event>)>,
    current_user: &Option<PublicKey>,
) -> Option<(&'a EventId, &'a (Event, Vec<Event>))> {
    open_proposals.iter().find(|(_, (proposal, _))| {
        is_event_proposal_root_for_branch(proposal, refstr, current_user).unwrap_or(false)
    })
}

pub fn join_with_and<T: ToString>(items: &[T]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        _ => {
            let last_item = items.last().unwrap().to_string();
            let rest = &items[..items.len() - 1];
            format!(
                "{} and {}",
                rest.iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                last_item
            )
        }
    }
}

/// get an ordered vector of server protocols to attempt
pub fn get_read_protocols_to_try(
    git_repo: &Repo,
    server_url: &CloneUrl,
    decoded_nostr_url: &NostrUrlDecoded,
) -> Vec<ServerProtocol> {
    if server_url.protocol() == ServerProtocol::Filesystem {
        vec![(ServerProtocol::Filesystem)]
    } else if let Some(protocol) = &decoded_nostr_url.protocol {
        vec![protocol.clone()]
    } else {
        let mut list = if server_url.protocol() == ServerProtocol::Http {
            vec![
                ServerProtocol::UnauthHttp,
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Http,
            ]
        } else if server_url.protocol() == ServerProtocol::Ftp {
            vec![ServerProtocol::Ftp, ServerProtocol::Ssh]
        } else {
            vec![
                ServerProtocol::UnauthHttps,
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Https,
            ]
        };
        if let Some(protocol) = get_protocol_preference(git_repo, server_url, &Direction::Fetch) {
            if let Some(pos) = list.iter().position(|p| *p == protocol) {
                list.remove(pos);
                list.insert(0, protocol);
            }
        }
        list
    }
}

/// get an ordered vector of server protocols to attempt
pub fn get_write_protocols_to_try(
    git_repo: &Repo,
    server_url: &CloneUrl,
    decoded_nostr_url: &NostrUrlDecoded,
) -> Vec<ServerProtocol> {
    if server_url.protocol() == ServerProtocol::Filesystem {
        vec![(ServerProtocol::Filesystem)]
    } else if let Some(protocol) = &decoded_nostr_url.protocol {
        vec![protocol.clone()]
    } else {
        let mut list = if server_url.protocol() == ServerProtocol::Http {
            vec![
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Http,
            ]
        } else if server_url.protocol() == ServerProtocol::Ftp {
            vec![ServerProtocol::Ssh, ServerProtocol::Ftp]
        } else {
            vec![
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Https,
            ]
        };
        if let Some(protocol) = get_protocol_preference(git_repo, server_url, &Direction::Push) {
            if let Some(pos) = list.iter().position(|p| *p == protocol) {
                list.remove(pos);
                list.insert(0, protocol);
            }
        }

        list
    }
}

#[derive(Debug, PartialEq)]
pub enum Direction {
    Push,
    Fetch,
}
impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Direction::Push => write!(f, "push"),
            Direction::Fetch => write!(f, "fetch"),
        }
    }
}

pub fn get_protocol_preference(
    git_repo: &Repo,
    server_url: &CloneUrl,
    direction: &Direction,
) -> Option<ServerProtocol> {
    let server_short_name = server_url.short_name();
    if let Ok(Some(list)) =
        git_repo.get_git_config_item(format!("nostr.protocol-{direction}").as_str(), Some(false))
    {
        for item in list.split(';') {
            let pair = item.split(',').collect::<Vec<&str>>();
            if let Some(url) = pair.get(1) {
                if *url == server_short_name {
                    if let Some(protocol) = pair.first() {
                        if let Ok(protocol) = ServerProtocol::from_str(protocol) {
                            return Some(protocol);
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn set_protocol_preference(
    git_repo: &Repo,
    protocol: &ServerProtocol,
    server_url: &CloneUrl,
    direction: &Direction,
) -> Result<()> {
    let server_short_name = server_url.short_name();
    let mut new = String::new();
    if let Some(list) =
        git_repo.get_git_config_item(format!("nostr.protocol-{direction}").as_str(), Some(false))?
    {
        for item in list.split(';') {
            let pair = item.split(',').collect::<Vec<&str>>();
            if let Some(url) = pair.get(1) {
                if *url != server_short_name && !item.is_empty() {
                    new.push_str(format!("{item};").as_str());
                }
            }
        }
    }
    new.push_str(format!("{protocol},{server_short_name};").as_str());

    git_repo.save_git_config_item(
        format!("nostr.protocol-{direction}").as_str(),
        new.as_str(),
        false,
    )
}

/// to understand whether to try over another protocol
pub fn fetch_or_list_error_is_not_authentication_failure(error: &anyhow::Error) -> bool {
    !error_might_be_authentication_related(error)
}

/// to understand whether to try over another protocol
pub fn push_error_is_not_authentication_failure(error: &anyhow::Error) -> bool {
    !error_might_be_authentication_related(error)
}

pub fn error_might_be_authentication_related(error: &anyhow::Error) -> bool {
    let error_str = error.to_string();
    for s in [
        "no ssh keys found",
        "invalid or unknown remote ssh hostkey",
        "authentication",
        "Permission",
        "permission",
        "not found",
    ] {
        if error_str.contains(s) {
            return true;
        }
    }
    false
}

fn count_lines_per_msg(width: u16, msg: &str, prefix_len: usize) -> usize {
    if width == 0 {
        return 1;
    }
    // ((msg_len+prefix) / width).ceil() implemented using Integer Arithmetic
    ((msg.chars().count() + prefix_len) + (width - 1) as usize) / width as usize
}

pub fn count_lines_per_msg_vec(width: u16, msgs: &[String], prefix_len: usize) -> usize {
    msgs.iter()
        .map(|msg| count_lines_per_msg(width, msg, prefix_len))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    mod join_with_and {
        use super::*;
        #[test]
        fn test_empty() {
            let items: Vec<&str> = vec![];
            assert_eq!(join_with_and(&items), "");
        }

        #[test]
        fn test_single_item() {
            let items = vec!["apple"];
            assert_eq!(join_with_and(&items), "apple");
        }

        #[test]
        fn test_two_items() {
            let items = vec!["apple", "banana"];
            assert_eq!(join_with_and(&items), "apple and banana");
        }

        #[test]
        fn test_three_items() {
            let items = vec!["apple", "banana", "cherry"];
            assert_eq!(join_with_and(&items), "apple, banana and cherry");
        }

        #[test]
        fn test_four_items() {
            let items = vec!["apple", "banana", "cherry", "date"];
            assert_eq!(join_with_and(&items), "apple, banana, cherry and date");
        }

        #[test]
        fn test_multiple_items() {
            let items = vec!["one", "two", "three", "four", "five"];
            assert_eq!(join_with_and(&items), "one, two, three, four and five");
        }
    }
}
