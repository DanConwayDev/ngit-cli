#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
// better solution to dead_code error on multiple binaries than https://stackoverflow.com/a/66196291
#![allow(dead_code)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use core::str;
use std::{
    collections::HashSet,
    env,
    io::{self},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use auth_git2::GitAuthenticator;
#[cfg(not(test))]
use client::Connect;
use client::{fetching_with_report, get_repo_ref_from_cache};
use git::RepoActions;
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
                temp_remote.download(&[refstr], None)?;
                temp_remote.disconnect()?;
                println!();
            }
            ["push", refspec] => {
                let auth = GitAuthenticator::default();
                auth.push(&git_repo.git_repo, &mut temp_remote, &[refspec])?;
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
