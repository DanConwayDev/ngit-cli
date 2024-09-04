use std::{collections::HashMap, io::Stdin};

use anyhow::{anyhow, bail, Result};
use auth_git2::GitAuthenticator;
use git2::Repository;
use ngit::{
    git::{Repo, RepoActions},
    git_events::tag_value,
    login::get_curent_user,
    repo_ref::RepoRef,
};

use crate::utils::{
    find_proposal_and_patches_by_branch_name, get_oids_from_fetch_batch, get_open_proposals,
    get_short_git_server_name, switch_clone_url_between_ssh_and_https,
};

pub async fn run_fetch(
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
