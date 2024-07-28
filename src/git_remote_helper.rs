#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
// better solution to dead_code error on multiple binaries than https://stackoverflow.com/a/66196291
#![allow(dead_code)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use core::str;
use std::{
    collections::HashSet,
    env,
    io::{self, Stdin},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use auth_git2::GitAuthenticator;
#[cfg(not(test))]
use client::Connect;
use client::{fetching_with_report, get_repo_ref_from_cache};
use git::RepoActions;
use git2::{Remote, Repository};
use nostr::nips::nip01::Coordinate;
use nostr_sdk::Url;

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
mod sub_commands;

#[tokio::main]
async fn main() -> Result<()> {
    let args = env::args();
    let args = args.skip(1).take(2).collect::<Vec<_>>();

    let ([_, url] | [url]) = args.as_slice() else {
        bail!("invalid arguments - no url");
    };
    if env::args().nth(1).as_deref() == Some("--version") {
        println!("v0.0.1");
    }

    let git_repo = Repo::from_path(&PathBuf::from(
        std::env::var("GIT_DIR").context("git should set GIT_DIR when remote helper is called")?,
    ))?;
    let git_repo_path = git_repo.get_path()?;

    #[cfg(not(test))]
    let client = Client::default();
    #[cfg(test)]
    let client = <MockConnect as std::default::Default>::default();

    let repo_coordinates = nostr_git_url_to_repo_coordinates(url).context("invalid nostr url")?;

    fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &repo_coordinates).await?;

    let stdin = io::stdin();
    let mut line = String::new();

    let temp_remote_url = repo_ref
        .git_server
        .first()
        .context("no git server listed in nostr repository announcement")?;

    let mut temp_remote = git_repo.git_repo.remote_anonymous(temp_remote_url)?;
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
            ["fetch", _oid, refstr] => {
                temp_remote.connect(git2::Direction::Fetch)?;
                temp_remote.download(&get_refstrs_from_fetch_batch(&stdin, refstr)?, None)?;
                temp_remote.disconnect()?;
                println!();
            }
            ["push", refspec] => {
                let auth = GitAuthenticator::default();
                let git_config = git_repo.git_repo.config()?;
                let mut push_options = git2::PushOptions::new();
                let mut remote_callbacks = git2::RemoteCallbacks::new();
                remote_callbacks.credentials(auth.credentials(&git_config));
                remote_callbacks.push_update_reference(|name, error| {
                    if let Some(error) = error {
                        println!("error {name} {error}");
                    } else {
                        println!("ok {name}",);
                    }
                    Ok(())
                });
                push_options.remote_callbacks(remote_callbacks);
                temp_remote.push(
                    &get_refspecs_from_push_batch(&stdin, refspec)?,
                    Some(&mut push_options),
                )?;
                update_remote_refs_from_push_refspecs(&git_repo.git_repo, refspec, url)?;
                temp_remote.disconnect()?;
                println!();
            }
            ["list"] => {
                temp_remote.connect(git2::Direction::Fetch)?;
                for head in temp_remote.list()? {
                    println!("{} {}", head.oid(), head.name());
                }
                temp_remote.disconnect()?;
                println!();
            }
            ["list", "for-push"] => {
                temp_remote.connect(git2::Direction::Fetch)?;
                for head in temp_remote.list()? {
                    if head.name() != "HEAD" {
                        println!("{} {}", head.oid(), head.name());
                    }
                }
                temp_remote.disconnect()?;
                println!();
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
    let coordinate = Coordinate::parse(Url::parse(url)?.domain().context("no naddr")?)?;
    if coordinate.kind.eq(&nostr_sdk::Kind::GitRepoAnnouncement) {
        repo_coordinattes.insert(coordinate);
    } else {
        bail!("naddr doesnt point to a git repository announcement");
    }
    Ok(repo_coordinattes)
}

fn update_remote_refs_from_push_refspecs(
    git_repo: &Repository,
    refspec: &str,
    url: &str,
) -> Result<()> {
    if !refspec.contains(':') {
        bail!(
            "refspec should contain a colon (:) but consists of: {}",
            refspec
        );
    }
    let parts = refspec.split(':').collect::<Vec<&str>>();
    let from = parts.first().unwrap();
    let to = parts.get(1).unwrap();

    let remote = get_remote_by_url(git_repo, url)?;

    let target_ref_name = format!(
        "refs/remotes/{}/{}",
        remote.name().context("remote should have a name")?,
        to.replace("refs/heads/", ""), // TODO only replace if it begins with this
    );
    if from.is_empty() {
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.delete()?;
        }
    } else {
        let local_ref = git_repo
            .find_reference(from)
            .context(format!("from ref in refspec should exist: {from}"))?;
        let commit = local_ref
            .peel_to_commit()
            .context(format!("from ref in refspec should peel to commit: {from}"))?;
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.set_target(commit.id(), "updated by nostr remote helper")?;
        } else {
            git_repo.reference(
                &target_ref_name,
                commit.id(),
                false,
                "created by nostr remote helper",
            )?;
        }
    }
    Ok(())
}

fn get_remote_by_url<'a>(git_repo: &'a Repository, url: &'a str) -> Result<Remote<'a>> {
    let remotes = git_repo.remotes()?;
    let remote_name = remotes
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
        .context("remote with matching url must be named")?;
    git_repo
        .find_remote(remote_name)
        .context("we should have just located this remote")
}

fn get_refstrs_from_fetch_batch(stdin: &Stdin, initial_refstr: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut refstrs = vec![initial_refstr.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["fetch", _oid, refstr] => {
                refstrs.push((*refstr).to_string());
            }
            [] => break,
            _ => bail!(
                "after a `fetch` command we are only expecting another fetch or an empty line"
            ),
        }
    }
    Ok(refstrs)
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
