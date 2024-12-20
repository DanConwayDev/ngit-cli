#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures, clippy::module_name_repetitions)]
// better solution to dead_code error on multiple binaries than https://stackoverflow.com/a/66196291
#![allow(dead_code)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use core::str;
use std::{
    collections::HashSet,
    env, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use client::{Connect, consolidate_fetch_reports, get_repo_ref_from_cache};
use git::{RepoActions, nostr_url::NostrUrlDecoded};
use ngit::{client, git, login::existing::load_existing_login};
use nostr::nips::nip01::Coordinate;
use utils::read_line;

use crate::{client::Client, git::Repo};

mod fetch;
mod list;
mod push;
mod utils;

#[tokio::main]
async fn main() -> Result<()> {
    let Some((decoded_nostr_url, git_repo)) = process_args().await? else {
        return Ok(());
    };

    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::default();

    if let Ok((signer, _, _)) = load_existing_login(
        &Some(&git_repo),
        &None,
        &None,
        &None,
        None,
        true,
        false,
        false,
    )
    .await
    {
        // signer for to respond to relay auth request
        client.set_signer(signer).await;
    }

    fetching_with_report_for_helper(git_repo_path, &client, &decoded_nostr_url.coordinate).await?;

    let mut repo_ref =
        get_repo_ref_from_cache(Some(git_repo_path), &decoded_nostr_url.coordinate).await?;

    repo_ref.set_nostr_git_url(decoded_nostr_url.clone());

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
                fetch::run_fetch(&git_repo, &repo_ref, &stdin, oid, refstr).await?;
            }
            ["push", refspec] => {
                push::run_push(
                    &git_repo,
                    &repo_ref,
                    &stdin,
                    refspec,
                    &client,
                    list_outputs.clone(),
                )
                .await?;
            }
            ["list"] => {
                list_outputs = Some(list::run_list(&git_repo, &repo_ref, false).await?);
            }
            ["list", "for-push"] => {
                list_outputs = Some(list::run_list(&git_repo, &repo_ref, true).await?);
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

async fn process_args() -> Result<Option<(NostrUrlDecoded, Repo)>> {
    let args = env::args();
    let args = args.skip(1).take(2).collect::<Vec<_>>();

    if env::args().nth(1).as_deref() == Some("--version") {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("v{VERSION}");
        return Ok(None);
    }

    let ([_, nostr_remote_url] | [nostr_remote_url]) = args.as_slice() else {
        println!("nostr plugin for git");
        println!("Usage:");
        println!(
            " - clone a nostr repository, or add as a remote, by using the url format nostr://pub123/identifier"
        );
        println!(
            " - remote branches beginning with `pr/` are open PRs from contributors; `ngit list` can be used to view all PRs"
        );
        println!(
            " - to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options"
        );
        println!("- publish a repository to nostr with `ngit init`");
        return Ok(None);
    };

    let git_repo = Repo::from_path(&PathBuf::from(
        std::env::var("GIT_DIR").context("git should set GIT_DIR when remote helper is called")?,
    ))?;

    let decoded_nostr_url = NostrUrlDecoded::parse_and_resolve(nostr_remote_url, &Some(&git_repo))
        .await
        .context("invalid nostr url")?;

    Ok(Some((decoded_nostr_url, git_repo)))
}

async fn fetching_with_report_for_helper(
    git_repo_path: &Path,
    client: &Client,
    trusted_maintainer_coordinate: &Coordinate,
) -> Result<()> {
    let term = console::Term::stderr();
    term.write_line("nostr: fetching...")?;
    let (relay_reports, progress_reporter) = client
        .fetch_all(
            Some(git_repo_path),
            Some(trusted_maintainer_coordinate),
            &HashSet::new(),
        )
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
