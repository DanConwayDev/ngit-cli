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

use anyhow::{anyhow, bail, Context, Result};
use auth_git2::GitAuthenticator;
use client::{
    consolidate_fetch_reports, get_events_from_cache, get_repo_ref_from_cache,
    get_state_from_cache, sign_event, Connect, STATE_KIND,
};
use console::Term;
use git::{sha1_to_oid, NostrUrlDecoded, RepoActions};
use git2::{Oid, Repository};
use nostr::nips::{nip01::Coordinate, nip10::Marker};
use nostr_sdk::{
    hashes::sha1::Hash as Sha1Hash, Event, EventBuilder, EventId, Kind, PublicKey, Tag, Url,
};
use nostr_signer::NostrSigner;
use repo_ref::RepoRef;
use repo_state::RepoState;
use sub_commands::{
    list::{
        get_all_proposal_patch_events_from_cache, get_commit_id_from_patch,
        get_most_recent_patch_with_ancestors, get_proposals_and_revisions_from_cache, status_kinds,
        tag_value,
    },
    send::{
        event_is_revision_root, event_to_cover_letter, generate_cover_letter_and_patch_events,
        generate_patch_event, send_events,
    },
};

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

    let decoded_nostr_url =
        NostrUrlDecoded::from_str(nostr_remote_url).context("invalid nostr url")?;

    fetching_with_report_for_helper(git_repo_path, &client, &decoded_nostr_url.coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &decoded_nostr_url.coordinates).await?;

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
            ["fetch", oid, refstr] => {
                fetch(&git_repo, &repo_ref, &stdin, oid, refstr).await?;
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

fn list_from_remotes(
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

fn switch_clone_url_between_ssh_and_https(url: &str) -> Result<String> {
    if url.starts_with("https://") {
        // Convert HTTPS to git@ syntax
        let parts: Vec<&str> = url.trim_start_matches("https://").split('/').collect();
        if parts.len() >= 2 {
            // Construct the git@ URL
            Ok(format!("git@{}:{}", parts[0], parts[1..].join("/")))
        } else {
            // If the format is unexpected, return an error
            bail!("Invalid HTTPS URL format: {}", url);
        }
    } else if url.starts_with("ssh://") {
        // Convert SSH to git@ syntax
        let parts: Vec<&str> = url.trim_start_matches("ssh://").split('/').collect();
        if parts.len() >= 2 {
            // Construct the git@ URL
            Ok(format!("git@{}:{}", parts[0], parts[1..].join("/")))
        } else {
            // If the format is unexpected, return an error
            bail!("Invalid SSH URL format: {}", url);
        }
    } else if url.starts_with("git@") {
        // Convert git@ syntax to HTTPS
        let parts: Vec<&str> = url.split(':').collect();
        if parts.len() == 2 {
            // Construct the HTTPS URL
            Ok(format!(
                "https://{}/{}",
                parts[0].trim_end_matches('@'),
                parts[1]
            ))
        } else {
            // If the format is unexpected, return an error
            bail!("Invalid git@ URL format: {}", url);
        }
    } else {
        // If the URL is neither HTTPS, SSH, nor git@, return an error
        bail!("Unsupported URL protocol: {}", url);
    }
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

async fn get_open_proposals(
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

fn get_curent_user(git_repo: &Repo) -> Result<Option<PublicKey>> {
    Ok(
        if let Some(npub) = git_repo.get_git_config_item("nostr.npub", None)? {
            if let Ok(public_key) = PublicKey::parse(npub) {
                Some(public_key)
            } else {
                None
            }
        } else {
            None
        },
    )
}

async fn get_all_proposals(
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

async fn fetch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    stdin: &Stdin,
    oid: &str,
    refstr: &str,
) -> Result<()> {
    let mut fetch_batch = get_oids_from_fetch_batch(stdin, oid, refstr)?;

    let oids_from_git_servers = fetch_batch
        .iter()
        .filter(|(refstr, _)| !refstr.contains("refs/heads/pr/"))
        .map(|(_, oid)| oid.clone())
        .collect::<Vec<String>>();

    let mut errors = HashMap::new();
    let term = console::Term::stderr();

    for git_server_url in &repo_ref.git_server {
        let term = console::Term::stderr();
        let short_name = get_short_git_server_name(git_repo, git_server_url);
        term.write_line(format!("fetching from {short_name}...").as_str())?;
        let res = fetch_from_git_server(&git_repo.git_repo, &oids_from_git_servers, git_server_url);
        term.clear_last_lines(1)?;
        if let Err(error1) = res {
            if let Ok(alternative_url) = switch_clone_url_between_ssh_and_https(git_server_url) {
                let res2 = fetch_from_git_server(
                    &git_repo.git_repo,
                    &oids_from_git_servers,
                    &alternative_url,
                );
                if let Err(error2) = res2 {
                    term.write_line(
                        format!(
                            "WARNING: failed to fetch from {short_name} error:{error1}\r\nand using alternative protocol {alternative_url}: {error2}"
                        ).as_str()
                    )?;
                    errors.insert(
                        short_name.to_string(),
                        anyhow!(
                            "{error1} and using alternative protocol {alternative_url}: {error2}"
                        ),
                    );
                } else {
                    break;
                }
            } else {
                term.write_line(
                    format!("WARNING: failed to fetch from {short_name} error:{error1}").as_str(),
                )?;
                errors.insert(short_name.to_string(), error1);
            }
        } else {
            break;
        }
    }

    if oids_from_git_servers
        .iter()
        .any(|oid| !git_repo.does_commit_exist(oid).unwrap())
        && !errors.is_empty()
    {
        bail!(
            "failed to fetch objects in nostr state event from:\r\n{}",
            errors
                .iter()
                .map(|(url, error)| format!("{url}: {error}"))
                .collect::<Vec<String>>()
                .join("\r\n")
        );
    }

    fetch_batch.retain(|refstr, _| refstr.contains("refs/heads/pr/"));

    if !fetch_batch.is_empty() {
        let open_proposals = get_open_proposals(git_repo, repo_ref).await?;

        let current_user = get_curent_user(git_repo)?;

        for (refstr, oid) in fetch_batch {
            if let Some((_, (_, patches))) =
                find_proposal_and_patches_by_branch_name(&refstr, &open_proposals, &current_user)
            {
                if !git_repo.does_commit_exist(&oid)? {
                    let mut patches_ancestor_first = patches.clone();
                    patches_ancestor_first.reverse();
                    if git_repo.does_commit_exist(&tag_value(
                        patches_ancestor_first.first().unwrap(),
                        "parent-commit",
                    )?)? {
                        for patch in &patches_ancestor_first {
                            git_repo.create_commit_from_patch(patch)?;
                        }
                    } else {
                        term.write_line(
                            format!("WARNING: cannot find parent commit for {refstr}").as_str(),
                        )?;
                    }
                }
            } else {
                term.write_line(format!("WARNING: cannot find proposal for {refstr}").as_str())?;
            }
        }
    }

    term.flush()?;
    println!();
    Ok(())
}

fn find_proposal_and_patches_by_branch_name<'a>(
    refstr: &'a str,
    open_proposals: &'a HashMap<EventId, (Event, Vec<Event>)>,
    current_user: &Option<PublicKey>,
) -> Option<(&'a EventId, &'a (Event, Vec<Event>))> {
    open_proposals.iter().find(|(_, (proposal, _))| {
        if let Ok(cl) = event_to_cover_letter(proposal) {
            if let Ok(mut branch_name) = cl.get_branch_name() {
                branch_name = if let Some(public_key) = current_user {
                    if proposal.author().eq(public_key) {
                        cl.branch_name.to_string()
                    } else {
                        branch_name
                    }
                } else {
                    branch_name
                };
                branch_name.eq(&refstr.replace("refs/heads/", ""))
            } else {
                false
            }
        } else {
            false
        }
    })
}

fn fetch_from_git_server(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
) -> Result<()> {
    let git_config = git_repo.config()?;

    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    // authentication may be required (and will be requird if clone url is ssh)
    let auth = GitAuthenticator::default();
    let mut fetch_options = git2::FetchOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    remote_callbacks.credentials(auth.credentials(&git_config));
    fetch_options.remote_callbacks(remote_callbacks);
    git_server_remote.download(oids, Some(&mut fetch_options))?;
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
    let refspecs = get_refspecs_from_push_batch(stdin, initial_refspec)?;

    let proposal_refspecs = refspecs
        .iter()
        .filter(|r| r.contains("refs/heads/pr/"))
        .cloned()
        .collect::<Vec<String>>();

    let mut git_server_refspecs = refspecs
        .iter()
        .filter(|r| !r.contains("refs/heads/pr/"))
        .cloned()
        .collect::<Vec<String>>();

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
        &git_server_refspecs,
        &existing_state,
        &list_outputs,
    )?;

    git_server_refspecs.retain(|refspec| {
        if let Some(rejected) = rejected_refspecs.get(&refspec.to_string()) {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            println!("error {to} {} out of sync with nostr", rejected.join(" "));
            false
        } else {
            true
        }
    });

    let mut events = vec![];

    if git_server_refspecs.is_empty() && proposal_refspecs.is_empty() {
        // all refspecs rejected
        println!();
        return Ok(());
    }

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

    if !repo_ref.maintainers.contains(&user_ref.public_key) {
        for refspec in &git_server_refspecs {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            println!(
                "error {to} your nostr account {} isn't listed as a maintainer of the repo",
                user_ref.metadata.name
            );
        }
        git_server_refspecs.clear();
        if proposal_refspecs.is_empty() {
            println!();
            return Ok(());
        }
    }

    if !git_server_refspecs.is_empty() {
        let new_state = generate_updated_state(git_repo, &existing_state, &git_server_refspecs)?;

        let new_repo_state =
            RepoState::build(repo_ref.identifier.clone(), new_state, &signer).await?;

        events.push(new_repo_state.event);

        for event in get_merged_status_events(
            &term,
            repo_ref,
            git_repo,
            nostr_remote_url,
            &signer,
            &git_server_refspecs,
        )
        .await?
        {
            events.push(event);
        }
    }

    let mut rejected_proposal_refspecs = vec![];
    if !proposal_refspecs.is_empty() {
        let all_proposals = get_all_proposals(git_repo, repo_ref).await?;
        let current_user = get_curent_user(git_repo)?;

        for refspec in &proposal_refspecs {
            let (from, to) = refspec_to_from_to(refspec).unwrap();
            let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;

            if let Some((_, (proposal, patches))) =
                find_proposal_and_patches_by_branch_name(to, &all_proposals, &current_user)
            {
                if [repo_ref.maintainers.clone(), vec![proposal.author()]]
                    .concat()
                    .contains(&user_ref.public_key)
                {
                    if refspec.starts_with('+') {
                        // force push
                        let (_, main_tip) = git_repo.get_main_or_master_branch()?;
                        let (mut ahead, _) =
                            git_repo.get_commits_ahead_behind(&main_tip, &tip_of_pushed_branch)?;
                        ahead.reverse();
                        for patch in generate_cover_letter_and_patch_events(
                            None,
                            git_repo,
                            &ahead,
                            &signer,
                            repo_ref,
                            &Some(proposal.id().to_string()),
                            &[],
                        )
                        .await?
                        {
                            events.push(patch);
                        }
                    } else {
                        // fast forward push
                        let tip_patch = patches.first().unwrap();
                        let tip_of_proposal = get_commit_id_from_patch(tip_patch)?;
                        let tip_of_proposal_commit =
                            git_repo.get_commit_or_tip_of_reference(&tip_of_proposal)?;

                        let (mut ahead, behind) = git_repo.get_commits_ahead_behind(
                            &tip_of_proposal_commit,
                            &tip_of_pushed_branch,
                        )?;
                        if behind.is_empty() {
                            let thread_id = if let Ok(root_event_id) = get_event_root(tip_patch) {
                                root_event_id
                            } else {
                                // tip patch is the root proposal
                                tip_patch.id()
                            };
                            let mut parent_patch = tip_patch.clone();
                            ahead.reverse();
                            for (i, commit) in ahead.iter().enumerate() {
                                let new_patch = generate_patch_event(
                                    git_repo,
                                    &git_repo.get_root_commit()?,
                                    commit,
                                    Some(thread_id),
                                    &signer,
                                    repo_ref,
                                    Some(parent_patch.id()),
                                    Some((
                                        (patches.len() + i + 1).try_into().unwrap(),
                                        (patches.len() + ahead.len()).try_into().unwrap(),
                                    )),
                                    None,
                                    &None,
                                    &[],
                                )
                                .await
                                .context("cannot make patch event from commit")?;
                                events.push(new_patch.clone());
                                parent_patch = new_patch;
                            }
                        } else {
                            // we shouldn't get here
                            term.write_line(
                                format!(
                                    "WARNING: failed to push {from} as nostr proposal. Try and force push ",
                                )
                                .as_str(),
                            )
                            .unwrap();
                            println!(
                                "error {to} cannot fastforward as newer patches found on proposal"
                            );
                            rejected_proposal_refspecs.push(refspec.to_string());
                        }
                    }
                } else {
                    println!(
                        "error {to} permission denied. you are not the proposal author or a repo maintainer"
                    );
                    rejected_proposal_refspecs.push(refspec.to_string());
                }
            } else {
                // TODO new proposal / couldn't find exisiting proposal
                let (_, main_tip) = git_repo.get_main_or_master_branch()?;
                let (mut ahead, _) =
                    git_repo.get_commits_ahead_behind(&main_tip, &tip_of_pushed_branch)?;
                ahead.reverse();
                for patch in generate_cover_letter_and_patch_events(
                    None,
                    git_repo,
                    &ahead,
                    &signer,
                    repo_ref,
                    &None,
                    &[],
                )
                .await?
                {
                    events.push(patch);
                }
            }
        }
    }

    // TODO check whether tip of each branch pushed is on at least one git server
    // before broadcasting the nostr state
    if !events.is_empty() {
        send_events(
            client,
            git_repo.get_path()?,
            events,
            user_ref.relays.write(),
            repo_ref.relays.clone(),
            false,
            true,
        )
        .await?;
    }

    for refspec in &[git_server_refspecs.clone(), proposal_refspecs.clone()].concat() {
        if rejected_proposal_refspecs.contains(refspec) {
            continue;
        }
        let (_, to) = refspec_to_from_to(refspec)?;
        println!("ok {to}");
        update_remote_refs_pushed(&git_repo.git_repo, refspec, nostr_remote_url)
            .context("could not update remote_ref locally")?;
    }

    // TODO make async - check gitlib2 callbacks work async
    for (git_server_url, remote_refspecs) in remote_refspecs {
        let remote_refspecs = remote_refspecs
            .iter()
            .filter(|refspec| git_server_refspecs.contains(refspec))
            .cloned()
            .collect::<Vec<String>>();
        if !refspecs.is_empty()
            && push_to_remote(git_repo, &git_server_url, &remote_refspecs, &term).is_err()
        {
            if let Ok(alternative_url) = switch_clone_url_between_ssh_and_https(&git_server_url) {
                if push_to_remote(git_repo, &alternative_url, &remote_refspecs, &term).is_err() {
                    // errors get printed as part of callback
                    // TODO prevent 2 warning messages and instead use one
                    // to say it didnt work over either https or ssh
                } else {
                    term.write_line(
                        format!("but succeed over alterantive protocol {alternative_url}",)
                            .as_str(),
                    )?;
                }
            }
        }
    }
    println!();
    Ok(())
}

fn push_to_remote(
    git_repo: &Repo,
    git_server_url: &str,
    remote_refspecs: &[String],
    term: &Term,
) -> Result<()> {
    let git_config = git_repo.git_repo.config()?;
    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_url)?;
    let auth = GitAuthenticator::default();
    let mut push_options = git2::PushOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    remote_callbacks.credentials(auth.credentials(&git_config));
    remote_callbacks.push_update_reference(|name, error| {
        if let Some(error) = error {
            term.write_line(
                format!(
                    "WARNING: {} failed to push {name} error: {error}",
                    get_short_git_server_name(git_repo, git_server_url),
                )
                .as_str(),
            )
            .unwrap();
        }
        Ok(())
    });
    push_options.remote_callbacks(remote_callbacks);
    git_server_remote.push(remote_refspecs, Some(&mut push_options))?;
    let _ = git_server_remote.disconnect();
    Ok(())
}

fn get_event_root(event: &nostr::Event) -> Result<EventId> {
    Ok(EventId::parse(
        event
            .tags()
            .iter()
            .find(|t| t.is_root())
            .context("no thread root in event")?
            .as_vec()
            .get(1)
            .unwrap(),
    )?)
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
        let short_name = get_short_git_server_name(git_repo, url);
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
                    } else if let Ok(remote_value_tip) =
                        git_repo.get_commit_or_tip_of_reference(remote_value)
                    {
                        if from_tip.eq(&remote_value_tip) {
                            // remote already at correct state
                            term.write_line(
                                format!("{short_name} {to} already up-to-date").as_str(),
                            )?;
                        }
                        let (ahead_of_local, behind_local) =
                            git_repo.get_commits_ahead_behind(&from_tip, &remote_value_tip)?;
                        if ahead_of_local.is_empty() {
                            // can soft push
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            // cant soft push
                            let (ahead_of_nostr, behind_nostr) = git_repo
                                .get_commits_ahead_behind(
                                    &git_repo.get_commit_or_tip_of_reference(nostr_value)?,
                                    &remote_value_tip,
                                )?;
                            if ahead_of_nostr.is_empty() {
                                // ancestor of nostr and we are force pushing anyway...
                                refspecs_for_remote.push(refspec.clone());
                            } else {
                                rejected_refspecs
                                    .entry(refspec.to_string())
                                    .and_modify(|a| a.push(url.to_string()))
                                    .or_insert(vec![url.to_string()]);
                                term.write_line(
                                    format!(
                                        "ERROR: {short_name} {to} conflicts with nostr ({} ahead {} behind) and local ({} ahead {} behind). either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote",
                                        ahead_of_nostr.len(),
                                        behind_nostr.len(),
                                        ahead_of_local.len(),
                                        behind_local.len(),
                                    ).as_str(),
                                )?;
                            }
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
                            format!("ERROR: {short_name} {to} conflicts with nostr and is not an ancestor of local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote").as_str(),
                        )?;
                    }
                } else {
                    // existing nostr branch not on remote
                    // report - creating new branch
                    term.write_line(
                        format!(
                            "{short_name} {to} doesn't exist and will be added as a new branch"
                        )
                        .as_str(),
                    )?;
                    refspecs_for_remote.push(refspec.clone());
                }
            } else if let Some(remote_value) = remote_value {
                // new to nostr but on remote
                if let Ok(remote_value_tip) = git_repo.get_commit_or_tip_of_reference(remote_value)
                {
                    let (ahead, behind) =
                        git_repo.get_commits_ahead_behind(&from_tip, &remote_value_tip)?;
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
                                        "ERROR: {short_name} already contains {to} {} ahead and {} behind local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote",
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
                        format!("ERROR: {short_name} already contains {to} at {remote_value} which is not an ancestor of local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote").as_str(),
                    )?;
                }
            } else {
                // in sync - new branch
                refspecs_for_remote.push(refspec.clone());
            }
        }
        if !refspecs_for_remote.is_empty() {
            refspecs_for_remotes.insert(url.to_string(), refspecs_for_remote);
        }
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
            if to.contains("refs/tags") {
                new_state.remove(&format!("{to}{}", "^{}"));
            }
        } else if to.contains("refs/tags") {
            new_state.insert(
                format!("{to}{}", "^{}"),
                git_repo
                    .get_commit_or_tip_of_reference(from)
                    .unwrap()
                    .to_string(),
            );
            new_state.insert(
                to.to_string(),
                git_repo
                    .git_repo
                    .find_reference(to)
                    .unwrap()
                    .peel(git2::ObjectType::Tag)
                    .unwrap()
                    .id()
                    .to_string(),
            );
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

async fn get_merged_status_events(
    term: &console::Term,
    repo_ref: &RepoRef,
    git_repo: &Repo,
    remote_nostr_url: &str,
    signer: &NostrSigner,
    refspecs_to_git_server: &Vec<String>,
) -> Result<Vec<Event>> {
    let mut events = vec![];
    for refspec in refspecs_to_git_server {
        let (from, to) = refspec_to_from_to(refspec)?;
        if to.eq("refs/heads/main") || to.eq("refs/heads/master") {
            let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;
            let Ok(tip_of_remote_branch) = git_repo.get_commit_or_tip_of_reference(
                &refspec_remote_ref_name(&git_repo.git_repo, refspec, remote_nostr_url)?,
            ) else {
                // branch not on remote
                continue;
            };
            let (ahead, _) =
                git_repo.get_commits_ahead_behind(&tip_of_remote_branch, &tip_of_pushed_branch)?;
            for commit_hash in ahead {
                let commit = git_repo.git_repo.find_commit(sha1_to_oid(&commit_hash)?)?;
                if commit.parent_count() > 1 {
                    // merge commit
                    for parent in commit.parents() {
                        // lookup parent id
                        let commit_events = get_events_from_cache(
                            git_repo.get_path()?,
                            vec![
                                nostr::Filter::default()
                                    .kind(nostr::Kind::GitPatch)
                                    .reference(parent.id().to_string()),
                            ],
                        )
                        .await?;
                        if let Some(commit_event) = commit_events.iter().find(|e| {
                            e.tags.iter().any(|t| {
                                t.as_vec()[0].eq("commit")
                                    && t.as_vec()[1].eq(&parent.id().to_string())
                            })
                        }) {
                            let (proposal_id, revision_id) =
                                get_proposal_and_revision_root_from_patch(git_repo, commit_event)
                                    .await?;
                            term.write_line(
                                format!(
                                    "merge commit {}: create nostr proposal status event",
                                    &commit.id().to_string()[..7],
                                )
                                .as_str(),
                            )?;

                            events.push(
                                create_merge_status(
                                    signer,
                                    repo_ref,
                                    &get_event_from_cache_by_id(git_repo, &proposal_id).await?,
                                    &if let Some(revision_id) = revision_id {
                                        Some(
                                            get_event_from_cache_by_id(git_repo, &revision_id)
                                                .await?,
                                        )
                                    } else {
                                        None
                                    },
                                    &commit_hash,
                                    commit_event.id(),
                                )
                                .await?,
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(events)
}

async fn get_event_from_cache_by_id(git_repo: &Repo, event_id: &EventId) -> Result<Event> {
    Ok(get_events_from_cache(
        git_repo.get_path()?,
        vec![nostr::Filter::default().id(*event_id)],
    )
    .await?
    .first()
    .context("cannot find event in cache")?
    .clone())
}

async fn create_merge_status(
    signer: &NostrSigner,
    repo_ref: &RepoRef,
    proposal: &Event,
    revision: &Option<Event>,
    merge_commit: &Sha1Hash,
    merged_patch: EventId,
) -> Result<Event> {
    let mut public_keys = repo_ref
        .maintainers
        .iter()
        .copied()
        .collect::<HashSet<PublicKey>>();
    public_keys.insert(proposal.author());
    if let Some(revision) = revision {
        public_keys.insert(revision.author());
    }
    sign_event(
        EventBuilder::new(
            nostr::event::Kind::GitStatusApplied,
            String::new(),
            [
                vec![
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                        vec!["git proposal merged / applied".to_string()],
                    ),
                    Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: proposal.id(),
                        relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::new),
                        marker: Some(Marker::Root),
                        public_key: None,
                    }),
                    Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: merged_patch,
                        relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::new),
                        marker: Some(Marker::Mention),
                        public_key: None,
                    }),
                ],
                if let Some(revision) = revision {
                    vec![Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: revision.id(),
                        relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::new),
                        marker: Some(Marker::Root),
                        public_key: None,
                    })]
                } else {
                    vec![]
                },
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| Tag::coordinate(c.clone()))
                    .collect::<Vec<Tag>>(),
                vec![
                    Tag::from_standardized(nostr::TagStandard::Reference(
                        repo_ref.root_commit.to_string(),
                    )),
                    Tag::from_standardized(nostr::TagStandard::Reference(format!(
                        "{merge_commit}"
                    ))),
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("merge-commit-id")),
                        vec![format!("{merge_commit}")],
                    ),
                ],
            ]
            .concat(),
        ),
        signer,
    )
    .await
}

async fn get_proposal_and_revision_root_from_patch(
    git_repo: &Repo,
    patch: &Event,
) -> Result<(EventId, Option<EventId>)> {
    let proposal_or_revision = if patch.tags.iter().any(|t| t.as_vec()[1].eq("root")) {
        patch.clone()
    } else {
        let proposal_or_revision_id = EventId::parse(
            if let Some(t) = patch.tags.iter().find(|t| t.is_root()) {
                t.clone()
            } else if let Some(t) = patch.tags.iter().find(|t| t.is_reply()) {
                t.clone()
            } else {
                Tag::event(patch.id())
            }
            .as_vec()[1]
                .clone(),
        )?;

        get_events_from_cache(
            git_repo.get_path()?,
            vec![nostr::Filter::default().id(proposal_or_revision_id)],
        )
        .await?
        .first()
        .unwrap()
        .clone()
    };

    if !proposal_or_revision.kind().eq(&Kind::GitPatch) {
        bail!("thread root is not a git patch");
    }

    if proposal_or_revision
        .tags
        .iter()
        .any(|t| t.as_vec()[1].eq("revision-root"))
    {
        Ok((
            EventId::parse(
                proposal_or_revision
                    .tags
                    .iter()
                    .find(|t| t.is_reply())
                    .unwrap()
                    .as_vec()[1]
                    .clone(),
            )?,
            Some(proposal_or_revision.id()),
        ))
    } else {
        Ok((proposal_or_revision.id(), None))
    }
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
    Ok((
        if parts.first().unwrap().starts_with('+') {
            &parts.first().unwrap()[1..]
        } else {
            parts.first().unwrap()
        },
        parts.get(1).unwrap(),
    ))
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
        to.replace("refs/heads/", ""), /* TODO only replace if it begins with this
                                        * TODO what about tags? */
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

fn get_short_git_server_name(git_repo: &Repo, url: &str) -> std::string::String {
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

fn get_oids_from_fetch_batch(
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

    mod nostr_git_url_paramemters_from_str {
        use git::ServerProtocol;
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
                NostrUrlDecoded::from_str(
                    "nostr://naddr1qqzxuemfwsqs6amnwvaz7tmwdaejumr0dspzpgqgmmc409hm4xsdd74sf68a2uyf9pwel4g9mfdg8l5244t6x4jdqvzqqqrhnym0k2qj"
                )?,
                NostrUrlDecoded {
                    coordinates: HashSet::from([Coordinate {
                        identifier: "ngit".to_string(),
                        public_key: PublicKey::parse(
                            "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                        )
                        .unwrap(),
                        kind: nostr_sdk::Kind::GitRepoAnnouncement,
                        relays: vec!["wss://nos.lol".to_string()], // wont add the slash
                    }]),
                    protocol: None,
                    user: None,
                },
            );
            Ok(())
        }
        mod from_npub_slash_identifier {
            use super::*;

            #[test]
            fn without_relay() -> Result<()> {
                assert_eq!(
                    NostrUrlDecoded::from_str(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                    )?,
                    NostrUrlDecoded {
                        coordinates: HashSet::from([get_model_coordinate(false)]),
                        protocol: None,
                        user: None,
                    },
                );
                Ok(())
            }

            mod with_url_parameters {

                use super::*;

                #[test]
                fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay=nos.lol"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_encoded_relay() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}",
                            urlencoding::encode("wss://nos.lol")
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_multiple_encoded_relays() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}&relay1={}",
                            urlencoding::encode("wss://nos.lol"),
                            urlencoding::encode("wss://relay.damus.io"),
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([Coordinate {
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
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_server_protocol() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?protocol=ssh"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_server_protocol_and_user() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?protocol=ssh&user=fred"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: Some("fred".to_string()),
                        },
                    );
                    Ok(())
                }
            }
            mod with_parameters_embedded_with_slashes {
                use super::*;

                #[test]
                fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/nos.lol/ngit"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_encoded_relay() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/{}/ngit",
                            urlencoding::encode("wss://nos.lol")
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_multiple_encoded_relays() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/{}/{}/ngit",
                            urlencoding::encode("wss://nos.lol"),
                            urlencoding::encode("wss://relay.damus.io"),
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([Coordinate {
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
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_server_protocol() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_server_protocol_and_user() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://fred@ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: Some("fred".to_string()),
                        },
                    );
                    Ok(())
                }
            }
        }
    }

    mod refspec_to_from_to {
        use super::*;

        #[test]
        fn trailing_plus_stripped() {
            let (from, _) = refspec_to_from_to("+testing:testingb").unwrap();
            assert_eq!(from, "testing");
        }
    }
}
