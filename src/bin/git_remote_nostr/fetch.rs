use std::io::Stdin;

use anyhow::{anyhow, bail, Context, Result};
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
    find_proposal_and_patches_by_branch_name, get_oids_from_fetch_batch, get_open_proposals,
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
            "failed to fetch objects in nostr state event from:\r\n{}",
            errors
                .iter()
                .map(std::string::ToString::to_string)
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

    // if protocol is local - just try local
    if server_url.protocol() == ServerProtocol::Local {
        let formatted_url = server_url.format_as(&ServerProtocol::Local, &None)?;
        term.write_line(format!("fetching from {formatted_url}...").as_str())?;
        if let Err(error) = fetch_from_git_server_url(git_repo, oids, &formatted_url) {
            term.write_line(
                format!("WARNING: failed to fetch from {formatted_url} error:{error}").as_str(),
            )?;
            return Err(error).context(format!("{formatted_url}: failed to fetch"));
        }
        return Ok(());
    }

    term.write_line(format!("fetching from {}...", server_url.domain()).as_str())?;

    // use overide protocol if specified
    if let Some(protocol) = &decoded_nostr_url.protocol {
        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;
        let res = fetch_from_git_server_url(git_repo, oids, &formatted_url);
        term.clear_last_lines(1)?;
        if let Err(error) = res {
            term.write_line(
                format!(
                    "WARNING: {formatted_url} failed to fetch over {protocol}{} as specified in nostr url. error:{error}",
                    if let Some(user) = &decoded_nostr_url.user {
                        format!(" with user '{user}'")
                    } else {
                        String::new()
                    }
                ).as_str(),
            )?;
            return Err(error).context(format!("{formatted_url}: failed to fetch"));
        }
        return Ok(());
    }

    // Try https unauthenticated
    let formatted_url = server_url.format_as(&ServerProtocol::Https, &None)?;
    let res = fetch_from_git_server_url_unauthenticated(git_repo, oids, &formatted_url);
    term.clear_last_lines(1)?;
    if let Err(unauth_error) = res {
        term.write_line(
            format!(
                "WARNING: {formatted_url} failed to fetch over unauthenticated https. {unauth_error}",
            ).as_str(),
        )?;
        // TODO what about timeout errors?
        // try over ssh
        let mut ssh_error = None;
        if check_ssh_keys() {
            term.write_line(format!("fetching from {} over ssh...", server_url.domain()).as_str())?;
            let formatted_url = server_url.format_as(&ServerProtocol::Ssh, &None)?;
            let res = fetch_from_git_server_url(git_repo, oids, &formatted_url);
            term.clear_last_lines(1)?;
            if let Err(error) = res {
                term.write_line(
                    format!("WARNING: {formatted_url} failed to fetch over ssh. error:{error}")
                        .as_str(),
                )?;
                term.write_line(
                    format!("fetching from {} over ssh...", server_url.domain()).as_str(),
                )?;
                ssh_error = Some(error);
            } else {
                return Ok(());
            }
        }
        // try over https authenticated
        term.write_line(
            format!(
                "fetching from {} over authenticated https...",
                server_url.domain()
            )
            .as_str(),
        )?;
        let formatted_url = server_url.format_as(&ServerProtocol::Ssh, &None)?;
        let res = fetch_from_git_server_url(git_repo, oids, &formatted_url);
        term.clear_last_lines(1)?;
        if let Err(auth_https_error) = res {
            term.write_line(
                format!("WARNING: {formatted_url} failed to fetch over authenticated https. error:{auth_https_error}",)
                    .as_str(),
            )?;
            let error_message = format!(
                "{} failed to fetch over unauthenticated https ({unauth_error}), ssh ({}) and authenticated https ({auth_https_error})",
                server_url.format_as(&ServerProtocol::Unspecified, &None)?,
                ssh_error.unwrap_or(anyhow!("no keys found"))
            );

            bail!(error_message)
        }
    }
    Ok(())
}

fn fetch_from_git_server_url(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
) -> Result<()> {
    let git_config = git_repo.config()?;
    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    let auth = GitAuthenticator::default();
    let mut fetch_options = git2::FetchOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    // TODO status update callback
    remote_callbacks.credentials(auth.credentials(&git_config));
    fetch_options.remote_callbacks(remote_callbacks);
    git_server_remote.download(oids, Some(&mut fetch_options))?;
    git_server_remote.disconnect()?;
    Ok(())
}

fn fetch_from_git_server_url_unauthenticated(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
) -> Result<()> {
    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    let mut fetch_options = git2::FetchOptions::new();
    let remote_callbacks = git2::RemoteCallbacks::new();
    // TODO status update callback
    fetch_options.remote_callbacks(remote_callbacks);
    git_server_remote.download(oids, Some(&mut fetch_options))?;
    git_server_remote.disconnect()?;
    Ok(())
}
