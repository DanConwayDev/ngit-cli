#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
// better solution to dead_code error on multiple binaries than https://stackoverflow.com/a/66196291
#![allow(dead_code)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use core::str;
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, Stdin},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use auth_git2::GitAuthenticator;
use client::{
    consolidate_fetch_reports, get_repo_ref_from_cache, get_state_from_cache, sign_event, Connect,
    STATE_KIND,
};
use git::RepoActions;
use git2::{Oid, Repository};
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{hashes::sha1::Hash as Sha1Hash, EventBuilder, PublicKey, Tag, Url};
use nostr_signer::NostrSigner;
use repo_ref::RepoRef;
use repo_state::RepoState;
use sub_commands::send::send_events;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::git::Repo;

mod cli;
mod cli_interactor;
mod client;
mod config;
mod git;
mod key_handling;
mod login;
mod repo_ref;
mod repo_state;
mod sub_commands;

#[tokio::main]
async fn main() -> Result<()> {
    let args = env::args();
    let args = args.skip(1).take(2).collect::<Vec<_>>();

    let ([_, nostr_remote_url] | [nostr_remote_url]) = args.as_slice() else {
        bail!("invalid arguments - no url");
    };
    if env::args().nth(1).as_deref() == Some("--version") {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("v{VERSION}");
        return Ok(());
    }

    let git_repo = Repo::from_path(&PathBuf::from(
        std::env::var("GIT_DIR").context("git should set GIT_DIR when remote helper is called")?,
    ))?;
    let git_repo_path = git_repo.get_path()?;

    #[cfg(not(test))]
    let client = Client::default();
    #[cfg(test)]
    let client = <MockConnect as std::default::Default>::default();

    let repo_coordinates =
        nostr_git_url_to_repo_coordinates(nostr_remote_url).context("invalid nostr url")?;

    fetching_with_report_for_helper(git_repo_path, &client, &repo_coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &repo_coordinates).await?;

    let stdin = io::stdin();
    let mut line = String::new();

    let mut list_outputs = None;
    loop {
        let tokens = read_line(&stdin, &mut line)?;

        match tokens.as_slice() {
            ["capabilities"] => {
                println!("option");
                println!("push");
                println!("fetch");
                println!();
            }
            ["option", "verbosity"] => {
                println!("ok");
            }
            ["option", ..] => {
                println!("unsupported");
            }
            ["fetch", oid, _refstr] => {
                fetch(&git_repo.git_repo, &repo_ref, &stdin, oid)?;
            }
            ["push", refspec] => {
                push(
                    &git_repo,
                    &repo_ref,
                    nostr_remote_url,
                    &stdin,
                    refspec,
                    &client,
                    list_outputs.clone(),
                )
                .await?;
            }
            ["list"] => {
                list_outputs = Some(list(&git_repo, &repo_ref, false).await?);
            }
            ["list", "for-push"] => {
                list_outputs = Some(list(&git_repo, &repo_ref, true).await?);
            }
            [] => {
                return Ok(());
            }
            _ => {
                bail!(format!("unknown command: {}", line.trim().to_owned()));
            }
        }
    }
}

/// Read one line from stdin, and split it into tokens.
pub(crate) fn read_line<'a>(stdin: &io::Stdin, line: &'a mut String) -> io::Result<Vec<&'a str>> {
    line.clear();

    let read = stdin.read_line(line)?;
    if read == 0 {
        return Ok(vec![]);
    }
    let line = line.trim();
    let tokens = line.split(' ').filter(|t| !t.is_empty()).collect();

    Ok(tokens)
}

fn nostr_git_url_to_repo_coordinates(url: &str) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinattes = HashSet::new();
    let url = Url::parse(url)?;

    if url.scheme().ne("nostr") {
        bail!("nostr git url must start with nostr://")
    }

    if let Ok(coordinate) = Coordinate::parse(url.domain().context("no naddr")?) {
        if coordinate.kind.eq(&nostr_sdk::Kind::GitRepoAnnouncement) {
            repo_coordinattes.insert(coordinate);
            return Ok(repo_coordinattes);
        }
        bail!("naddr doesnt point to a git repository announcement");
    }

    if let Some(domain) = url.domain() {
        if let Ok(public_key) = PublicKey::parse(domain) {
            if url.path().len() < 2 {
                bail!(
                    "nostr git url should include the repo identifier eg nostr://npub123/the-repo-identifer"
                );
            }
            let mut relays = vec![];
            for (name, value) in url.query_pairs() {
                if name.contains("relay") {
                    let mut decoded = urlencoding::decode(&value)
                        .context("could not parse relays in nostr git url")?
                        .to_string();
                    if !decoded.starts_with("ws://") && !decoded.starts_with("wss://") {
                        decoded = format!("wss://{decoded}");
                    }
                    let url =
                        Url::parse(&decoded).context("could not parse relays in nostr git url")?;
                    relays.push(url.to_string());
                }
            }
            repo_coordinattes.insert(Coordinate {
                identifier: url.path()[1..].to_string(),
                public_key,
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays,
            });
            return Ok(repo_coordinattes);
        }
    }
    bail!(
        "nostr git url must be in format nostr://naddr123 or nostr://npub123/identifer?relay=wss://relay-example.com&relay1=wss://relay-example.org"
    );
}

async fn fetching_with_report_for_helper(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_coordinates: &HashSet<Coordinate>,
) -> Result<()> {
    let term = console::Term::stderr();
    term.write_line("nostr: fetching...")?;
    let (relay_reports, progress_reporter) = client
        .fetch_all(git_repo_path, repo_coordinates, &HashSet::new())
        .await?;
    if !relay_reports.iter().any(std::result::Result::is_err) {
        let _ = progress_reporter.clear();
        term.clear_last_lines(1)?;
    }
    let report = consolidate_fetch_reports(relay_reports);
    if report.to_string().is_empty() {
        term.write_line("nostr: no updates")?;
    } else {
        term.write_line(&format!("nostr updates: {report}"))?;
    }
    Ok(())
}

async fn list(
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

    let state = if let Some(nostr_state) = nostr_state {
        for (name, value) in &nostr_state.state {
            for (url, remote_state) in &remote_states {
                if let Some(remote_value) = remote_state.get(name) {
                    if value.ne(remote_value) {
                        term.write_line(
                            format!(
                                "WARNING: {url} {name} is {} nostr ",
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
                        format!("WARNING: {url} {name} is missing but tracked on nostr").as_str(),
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

fn list_from_remotes(
    term: &console::Term,
    git_repo: &Repo,
    git_servers: &Vec<String>,
) -> Result<HashMap<String, HashMap<String, String>>> {
    let mut remote_states = HashMap::new();
    for url in git_servers {
        term.write_line(format!("fetching refs list: {url}...").as_str())?;
        match list_from_remote(git_repo, url) {
            Ok(remote_state) => {
                remote_states.insert(url.clone(), remote_state);
            }
            Err(error) => {
                term.write_line(
                    format!("WARNING: {url} failed to list refs error: {error}").as_str(),
                )?;
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
    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_remote_url)?;
    git_server_remote.connect(git2::Direction::Fetch)?;
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

fn fetch(git_repo: &Repository, repo_ref: &RepoRef, stdin: &Stdin, oid: &str) -> Result<()> {
    let oids = get_oids_from_fetch_batch(stdin, oid)?;

    let mut errors = HashMap::new();
    for git_server_url in &repo_ref.git_server {
        if let Err(e) = fetch_from_git_server(git_repo, &oids, git_server_url) {
            errors.insert(git_server_url.to_string(), e);
        } else {
            println!();
            return Ok(());
        }
    }
    bail!(
        "failed to fetch objects in nostr state event from:\r\n{}",
        errors
            .iter()
            .map(|(url, error)| format!("{url}: {error}"))
            .collect::<Vec<String>>()
            .join("\r\n")
    );
}

fn fetch_from_git_server(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
) -> Result<()> {
    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    git_server_remote.connect(git2::Direction::Fetch)?;
    git_server_remote.download(oids, None)?;
    git_server_remote.disconnect()?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn push(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    nostr_remote_url: &str,
    stdin: &Stdin,
    initial_refspec: &str,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    list_outputs: Option<HashMap<String, HashMap<String, String>>>,
) -> Result<()> {
    let mut refspecs = get_refspecs_from_push_batch(stdin, initial_refspec)?;

    let term = console::Term::stderr();

    let list_outputs = match list_outputs {
        Some(outputs) => outputs,
        _ => list_from_remotes(&term, git_repo, &repo_ref.git_server)?,
    };

    let nostr_state = get_state_from_cache(git_repo.get_path()?, repo_ref).await;

    let existing_state = {
        // if no state events - create from first git server listed
        if let Ok(nostr_state) = &nostr_state {
            nostr_state.state.clone()
        } else if let Some(url) = repo_ref
            .git_server
            .iter()
            .find(|&url| list_outputs.contains_key(url))
        {
            list_outputs.get(url).unwrap().to_owned()
        } else {
            bail!(
                "cannot connect to git servers: {}",
                repo_ref.git_server.join(" ")
            );
        }
    };

    let (rejected_refspecs, remote_refspecs) = create_rejected_refspecs_and_remotes_refspecs(
        &term,
        git_repo,
        &refspecs,
        &existing_state,
        &list_outputs,
    )?;

    refspecs.retain(|refspec| {
        if let Some(rejected) = rejected_refspecs.get(&refspec.to_string()) {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            println!("error {to} {} out of sync with nostr", rejected.join(" "));
            false
        } else {
            true
        }
    });

    if refspecs.is_empty() {
        // all refspecs rejected
        println!();
        return Ok(());
    }

    let new_state = generate_updated_state(git_repo, &existing_state, &refspecs)?;

    let (signer, user_ref) = login::launch(
        git_repo,
        &None,
        &None,
        &None,
        &None,
        Some(client),
        false,
        true,
    )
    .await?;

    let new_repo_state = RepoState::build(repo_ref.identifier.clone(), new_state, &signer).await?;

    // TODO check whether tip of each branch pushed is on at least one git server
    // before broadcasting the nostr state

    send_events(
        client,
        git_repo.get_path()?,
        vec![new_repo_state.event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        false,
        true,
    )
    .await?;

    for refspec in &refspecs {
        let (_, to) = refspec_to_from_to(refspec)?;
        println!("ok {to}");
        update_remote_refs_pushed(&git_repo.git_repo, refspec, nostr_remote_url)
            .context("could not update remote_ref locally")?;
    }

    // TODO make async - check gitlib2 callbacks work async
    let git_config = git_repo.git_repo.config()?;
    for (git_server_url, refspecs) in remote_refspecs {
        if !refspecs.is_empty() {
            if let Ok(mut git_server_remote) = git_repo.git_repo.remote_anonymous(&git_server_url) {
                let auth = GitAuthenticator::default();
                let mut push_options = git2::PushOptions::new();
                let mut remote_callbacks = git2::RemoteCallbacks::new();
                remote_callbacks.credentials(auth.credentials(&git_config));
                remote_callbacks.push_update_reference(|name, error| {
                    if let Some(error) = error {
                        term.write_line(
                            format!(
                                "WARNING: {git_server_url} failed to push {name} error: {error}"
                            )
                            .as_str(),
                        )
                        .unwrap();
                    }
                    Ok(())
                });
                push_options.remote_callbacks(remote_callbacks);
                let _ = git_server_remote.push(&refspecs, Some(&mut push_options));
                let _ = git_server_remote.disconnect();
            }
        }
    }
    println!();
    Ok(())
}

type HashMapUrlRefspecs = HashMap<String, Vec<String>>;

#[allow(clippy::too_many_lines)]
fn create_rejected_refspecs_and_remotes_refspecs(
    term: &console::Term,
    git_repo: &Repo,
    refspecs: &Vec<String>,
    nostr_state: &HashMap<String, String>,
    list_outputs: &HashMap<String, HashMap<String, String>>,
) -> Result<(HashMapUrlRefspecs, HashMapUrlRefspecs)> {
    let mut refspecs_for_remotes = HashMap::new();

    let mut rejected_refspecs: HashMapUrlRefspecs = HashMap::new();

    for (url, remote_state) in list_outputs {
        let mut refspecs_for_remote = vec![];
        for refspec in refspecs {
            let (from, to) = refspec_to_from_to(refspec)?;
            let nostr_value = nostr_state.get(to);
            let remote_value = remote_state.get(to);
            if from.is_empty() {
                if remote_value.is_some() {
                    // delete remote branch
                    refspecs_for_remote.push(refspec.clone());
                }
                continue;
            }
            let from_tip = git_repo.get_commit_or_tip_of_reference(from)?;
            if let Some(nostr_value) = nostr_value {
                if let Some(remote_value) = remote_value {
                    if nostr_value.eq(remote_value) {
                        // in sync - existing branch at same state
                        let is_remote_tip_ancestor_of_commit = if let Ok(remote_value_tip) =
                            git_repo.get_commit_or_tip_of_reference(remote_value)
                        {
                            if let Ok((_, behind)) =
                                git_repo.get_commits_ahead_behind(&remote_value_tip, &from_tip)
                            {
                                behind.is_empty()
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if is_remote_tip_ancestor_of_commit {
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            // this is a force push so we need to force push to git server too
                            if refspec.starts_with('+') {
                                refspecs_for_remote.push(refspec.clone());
                            } else {
                                refspecs_for_remote.push(format!("+{refspec}"));
                            }
                        }
                        // TODO do we need to force push to this remote?
                    } else if let Ok(remote_value_tip) =
                        git_repo.get_commit_or_tip_of_reference(remote_value)
                    {
                        if from_tip.eq(&remote_value_tip) {
                            // remote already at correct state
                            term.write_line(format!("{url} {to} already up-to-date").as_str())?;
                        }
                        let nostr_value_tip =
                            git_repo.get_commit_or_tip_of_reference(nostr_value)?;
                        let (ahead, behind) = git_repo
                            .get_commits_ahead_behind(&remote_value_tip, &nostr_value_tip)?;
                        if behind.is_empty() {
                            // can soft push
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            // cant soft push
                            rejected_refspecs
                                .entry(refspec.to_string())
                                .and_modify(|a| a.push(url.to_string()))
                                .or_insert(vec![url.to_string()]);
                            term.write_line(
                                format!(
                                    "ERROR: {url} {to} conflicts with nostr ({} ahead {} behind). either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote",
                                    ahead.len(),
                                    behind.len(),
                                ).as_str(),
                            )?;
                        };
                    } else {
                        // remote_value oid is not present locally
                        // TODO can we download the remote reference?

                        // cant soft push
                        rejected_refspecs
                            .entry(refspec.to_string())
                            .and_modify(|a| a.push(url.to_string()))
                            .or_insert(vec![url.to_string()]);
                        term.write_line(
                            format!("ERROR: {url} {to} conflicts with nostr and is not an ancestor of local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote").as_str(),
                        )?;
                    }
                } else {
                    // existing nostr branch not on remote
                    // report - creating new branch
                    term.write_line(
                        format!("{url} {to} doesn't exist and will be added as a new branch")
                            .as_str(),
                    )?;
                    refspecs_for_remote.push(refspec.clone());
                }
            } else if let Some(remote_value) = remote_value {
                // new to nostr but on remote
                if let Ok(remote_value_tip) = git_repo.get_commit_or_tip_of_reference(remote_value)
                {
                    let (ahead, behind) =
                        git_repo.get_commits_ahead_behind(&remote_value_tip, &from_tip)?;
                    if behind.is_empty() {
                        // can soft push
                        refspecs_for_remote.push(refspec.clone());
                    } else {
                        // cant soft push
                        rejected_refspecs
                            .entry(refspec.to_string())
                            .and_modify(|a| a.push(url.to_string()))
                            .or_insert(vec![url.to_string()]);
                        term.write_line(
                                    format!(
                                        "ERROR: {url} already contains {to} {} ahead and {} behind local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote",
                                        ahead.len(),
                                        behind.len(),
                                    ).as_str(),
                                )?;
                    }
                } else {
                    // havn't fetched oid from remote
                    // TODO fetch oid from remote
                    // cant soft push
                    rejected_refspecs
                        .entry(refspec.to_string())
                        .and_modify(|a| a.push(url.to_string()))
                        .or_insert(vec![url.to_string()]);
                    term.write_line(
                        format!("ERROR: {url} already contains {to} at {remote_value} which is not an ancestor of local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote").as_str(),
                    )?;
                }
            } else {
                // in sync - new branch
                refspecs_for_remote.push(refspec.clone());
            }
        }
        refspecs_for_remotes.insert(url.to_string(), refspecs_for_remote);
    }

    // remove rejected refspecs so they dont get pushed to some remotes
    let mut remotes_refspecs_without_rejected = HashMap::new();
    for (url, value) in &refspecs_for_remotes {
        remotes_refspecs_without_rejected.insert(
            url.to_string(),
            value
                .iter()
                .filter(|refspec| !rejected_refspecs.contains_key(*refspec))
                .cloned()
                .collect(),
        );
    }
    Ok((rejected_refspecs, remotes_refspecs_without_rejected))
}

fn generate_updated_state(
    git_repo: &Repo,
    existing_state: &HashMap<String, String>,
    refspecs: &Vec<String>,
) -> Result<HashMap<String, String>> {
    let mut new_state = existing_state.clone();

    for refspec in refspecs {
        let (from, to) = refspec_to_from_to(refspec)?;
        if from.is_empty() {
            // delete
            new_state.remove(to);
        } else {
            // add or update
            new_state.insert(
                to.to_string(),
                git_repo
                    .get_commit_or_tip_of_reference(from)
                    .unwrap()
                    .to_string(),
            );
        }
    }
    Ok(new_state)
}

fn update_remote_refs_pushed(
    git_repo: &Repository,
    refspec: &str,
    nostr_remote_url: &str,
) -> Result<()> {
    let (from, _) = refspec_to_from_to(refspec)?;

    let target_ref_name = refspec_remote_ref_name(git_repo, refspec, nostr_remote_url)?;

    if from.is_empty() {
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.delete()?;
        }
    } else {
        let commit = reference_to_commit(git_repo, from)
            .context(format!("cannot get commit of reference {from}"))?;
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.set_target(commit, "updated by nostr remote helper")?;
        } else {
            git_repo.reference(
                &target_ref_name,
                commit,
                false,
                "created by nostr remote helper",
            )?;
        }
    }
    Ok(())
}

fn refspec_to_from_to(refspec: &str) -> Result<(&str, &str)> {
    if !refspec.contains(':') {
        bail!(
            "refspec should contain a colon (:) but consists of: {}",
            refspec
        );
    }
    let parts = refspec.split(':').collect::<Vec<&str>>();
    Ok((parts.first().unwrap(), parts.get(1).unwrap()))
}

fn refspec_remote_ref_name(
    git_repo: &Repository,
    refspec: &str,
    nostr_remote_url: &str,
) -> Result<String> {
    let (_, to) = refspec_to_from_to(refspec)?;
    let nostr_remote = git_repo
        .find_remote(&get_remote_name_by_url(git_repo, nostr_remote_url)?)
        .context("we should have just located this remote")?;
    Ok(format!(
        "refs/remotes/{}/{}",
        nostr_remote.name().context("remote should have a name")?,
        to.replace("refs/heads/", ""), // TODO only replace if it begins with this
    ))
}

fn reference_to_commit(git_repo: &Repository, reference: &str) -> Result<Oid> {
    Ok(git_repo
        .find_reference(reference)
        .context(format!("cannot find reference: {reference}"))?
        .peel_to_commit()
        .context(format!("cannot get commit from reference: {reference}"))?
        .id())
}

// this maybe a commit id or a ref: pointer
fn reference_to_ref_value(git_repo: &Repository, reference: &str) -> Result<String> {
    let reference_obj = git_repo
        .find_reference(reference)
        .context(format!("cannot find reference: {reference}"))?;
    if let Some(symref) = reference_obj.symbolic_target() {
        Ok(symref.to_string())
    } else {
        Ok(reference_obj
            .peel_to_commit()
            .context(format!("cannot get commit from reference: {reference}"))?
            .id()
            .to_string())
    }
}

fn get_remote_name_by_url(git_repo: &Repository, url: &str) -> Result<String> {
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

fn get_oids_from_fetch_batch(stdin: &Stdin, initial_oid: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut oids = vec![initial_oid.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["fetch", oid, _refstr] => {
                oids.push((*oid).to_string());
            }
            [] => break,
            _ => bail!(
                "after a `fetch` command we are only expecting another fetch or an empty line"
            ),
        }
    }
    Ok(oids)
}

fn get_refspecs_from_push_batch(stdin: &Stdin, initial_refspec: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut refspecs = vec![initial_refspec.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["push", spec] => {
                refspecs.push((*spec).to_string());
            }
            [] => break,
            _ => {
                bail!("after a `push` command we are only expecting another push or an empty line")
            }
        }
    }
    Ok(refspecs)
}

impl RepoState {
    pub async fn build(
        identifier: String,
        state: HashMap<String, String>,
        signer: &NostrSigner,
    ) -> Result<RepoState> {
        let mut tags = vec![Tag::identifier(identifier.clone())];
        for (name, value) in &state {
            tags.push(Tag::custom(
                nostr_sdk::TagKind::Custom(name.into()),
                vec![value.clone()],
            ));
        }
        let event = sign_event(EventBuilder::new(STATE_KIND, "", tags), signer).await?;
        Ok(RepoState {
            identifier,
            state,
            event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod nostr_git_url_to_repo_coordinates {
        use nostr_sdk::PublicKey;

        use super::*;

        fn get_model_coordinate(relays: bool) -> Coordinate {
            Coordinate {
                identifier: "ngit".to_string(),
                public_key: PublicKey::parse(
                    "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                )
                .unwrap(),
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays: if relays {
                    vec!["wss://nos.lol/".to_string()]
                } else {
                    vec![]
                },
            }
        }

        #[test]
        fn from_naddr() -> Result<()> {
            assert_eq!(
                nostr_git_url_to_repo_coordinates(
                    "nostr://naddr1qqzxuemfwsqs6amnwvaz7tmwdaejumr0dspzpgqgmmc409hm4xsdd74sf68a2uyf9pwel4g9mfdg8l5244t6x4jdqvzqqqrhnym0k2qj"
                )?,
                HashSet::from([Coordinate {
                    identifier: "ngit".to_string(),
                    public_key: PublicKey::parse(
                        "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                    )
                    .unwrap(),
                    kind: nostr_sdk::Kind::GitRepoAnnouncement,
                    relays: vec!["wss://nos.lol".to_string()], // wont add the slash
                }]),
            );
            Ok(())
        }
        mod from_npub_slah_identifier {
            use super::*;

            #[test]
            fn without_relay() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                    )?,
                    HashSet::from([get_model_coordinate(false)]),
                );
                Ok(())
            }

            #[test]
            fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay=nos.lol"
                    )?,
                    HashSet::from([get_model_coordinate(true)]),
                );
                Ok(())
            }

            #[test]
            fn with_encoded_relay() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(&format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}",
                        urlencoding::encode("wss://nos.lol")
                    ))?,
                    HashSet::from([get_model_coordinate(true)]),
                );
                Ok(())
            }
            #[test]
            fn with_multiple_encoded_relays() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(&format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}&relay1={}",
                        urlencoding::encode("wss://nos.lol"),
                        urlencoding::encode("wss://relay.damus.io"),
                    ))?,
                    HashSet::from([Coordinate {
                        identifier: "ngit".to_string(),
                        public_key: PublicKey::parse(
                            "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                        )
                        .unwrap(),
                        kind: nostr_sdk::Kind::GitRepoAnnouncement,
                        relays: vec![
                            "wss://nos.lol/".to_string(),
                            "wss://relay.damus.io/".to_string(),
                        ],
                    }]),
                );
                Ok(())
            }
        }
    }
}
