use core::str;
use std::io::Stdin;

use anyhow::{anyhow, bail, Result};
use auth_git2::GitAuthenticator;
use git2::Repository;
use ngit::{
    git::{
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        utils::check_ssh_keys,
        Repo, RepoActions,
    },
    git_events::tag_value,
    login::get_curent_user,
    repo_ref::RepoRef,
};

use crate::utils::{
    fetch_or_list_error_is_not_authentication_failure, find_proposal_and_patches_by_branch_name,
    get_oids_from_fetch_batch, get_open_proposals, get_read_protocols_to_try, join_with_and,
    report_on_sideband_progress, report_on_transfer_progress, ProgressStatus, TransferDirection,
};

pub async fn run_fetch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    decoded_nostr_url: &NostrUrlDecoded,
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

    let mut errors = vec![];
    let term = console::Term::stderr();

    for git_server_url in &repo_ref.git_server {
        let term = console::Term::stderr();
        if let Err(error) = fetch_from_git_server(
            &git_repo.git_repo,
            &oids_from_git_servers,
            git_server_url,
            decoded_nostr_url,
            &term,
        ) {
            errors.push(error);
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
            "fetch: failed to fetch objects in nostr state event from:\r\n{}",
            errors
                .iter()
                .map(|e| format!(" - {e}"))
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

fn fetch_from_git_server(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    term: &console::Term,
) -> Result<()> {
    let server_url = git_server_url.parse::<CloneUrl>()?;

    let protocols_to_attempt = get_read_protocols_to_try(&server_url, decoded_nostr_url);

    let mut failed_protocols = vec![];
    let mut success = false;
    for protocol in &protocols_to_attempt {
        term.write_line(
            format!("fetching {} over {protocol}...", server_url.short_name(),).as_str(),
        )?;

        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;
        let res = fetch_from_git_server_url(
            git_repo,
            oids,
            &formatted_url,
            [ServerProtocol::UnauthHttps, ServerProtocol::UnauthHttp].contains(protocol),
            term,
        );
        term.clear_last_lines(1)?;
        if let Err(error) = res {
            term.write_line(
                format!("fetch: {formatted_url} failed over {protocol}: {error}").as_str(),
            )?;
            failed_protocols.push(protocol);
            if protocol == &ServerProtocol::Ssh
                && fetch_or_list_error_is_not_authentication_failure(&error)
            {
                // authenticated by failed to complete request
                break;
            }
        } else {
            success = true;
            if !failed_protocols.is_empty() {
                term.write_line(format!("fetch: succeeded over {protocol}").as_str())?;
            }
            break;
        }
    }
    if success {
        Ok(())
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
        term.write_line(format!("fetch: {error}").as_str())?;
        Err(error)
    }
}

fn fetch_from_git_server_url(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
    dont_authenticate: bool,
    term: &console::Term,
) -> Result<()> {
    if git_server_url.parse::<CloneUrl>()?.protocol() == ServerProtocol::Ssh && !check_ssh_keys() {
        bail!("no ssh keys found");
    }
    let git_config = git_repo.config()?;
    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    let auth = GitAuthenticator::default();
    let mut fetch_options = git2::FetchOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    remote_callbacks.sideband_progress(|data| {
        report_on_sideband_progress(data, term);
        true
    });
    remote_callbacks.transfer_progress(|stats| {
        let _ = term.clear_last_lines(1);
        report_on_transfer_progress(
            &stats,
            term,
            TransferDirection::Fetch,
            ProgressStatus::InProgress,
        );
        true
    });

    if !dont_authenticate {
        remote_callbacks.credentials(auth.credentials(&git_config));
    }
    fetch_options.remote_callbacks(remote_callbacks);
    term.write_line("")?;
    git_server_remote.download(oids, Some(&mut fetch_options))?;

    report_on_transfer_progress(
        &git_server_remote.stats(),
        term,
        TransferDirection::Fetch,
        ProgressStatus::Complete,
    );

    git_server_remote.disconnect()?;
    Ok(())
}
