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
    str::FromStr,
};

use anyhow::{bail, Context, Result};
use client::{consolidate_fetch_reports, get_repo_ref_from_cache, Connect};
use git::{nostr_url::NostrUrlDecoded, RepoActions};
use ngit::{client, git};
use nostr::nips::nip01::Coordinate;
use utils::read_line;

use crate::{client::Client, git::Repo};

mod fetch;
mod list;
mod push;
mod utils;

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

    let client = Client::default();

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
                fetch::run_fetch(
                    &git_repo,
                    &repo_ref,
                    &decoded_nostr_url,
                    &stdin,
                    oid,
                    refstr,
                )
                .await?;
            }
            ["push", refspec] => {
                push::run_push(
                    &git_repo,
                    &repo_ref,
                    &decoded_nostr_url,
                    &stdin,
                    refspec,
                    &client,
                    list_outputs.clone(),
                )
                .await?;
            }
            ["list"] => {
                list_outputs =
                    Some(list::run_list(&git_repo, &repo_ref, &decoded_nostr_url, false).await?);
            }
            ["list", "for-push"] => {
                list_outputs =
                    Some(list::run_list(&git_repo, &repo_ref, &decoded_nostr_url, true).await?);
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

async fn fetching_with_report_for_helper(
    git_repo_path: &Path,
    client: &Client,
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
