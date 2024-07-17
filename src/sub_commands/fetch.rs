use std::collections::HashSet;

use anyhow::{Context, Result};
use clap;
use nostr::nips::nip01::Coordinate;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::{consolidate_fetch_reports, Connect},
    git::{Repo, RepoActions},
    repo_ref::get_repo_coordinates,
    Cli,
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// address pointer to repo announcement
    #[arg(long, action)]
    repo: Vec<String>,
}

pub async fn launch(args: &Cli, command_args: &SubCommandArgs) -> Result<()> {
    let _ = args;
    let git_repo = Repo::discover().context("cannot find a git repository")?;
    #[cfg(not(test))]
    let client = Client::default();
    #[cfg(test)]
    let client = <MockConnect as std::default::Default>::default();
    let repo_coordinates = if command_args.repo.is_empty() {
        get_repo_coordinates(&git_repo, &client).await?
    } else {
        let mut repo_coordinates = HashSet::new();
        for repo in &command_args.repo {
            repo_coordinates.insert(Coordinate::parse(repo.clone())?);
        }
        repo_coordinates
    };
    println!("fetching updates...");
    let (relay_reports, _) = client
        .fetch_all(git_repo.get_path()?, &repo_coordinates, &HashSet::new())
        .await?;
    let report = consolidate_fetch_reports(relay_reports);
    if report.to_string().is_empty() {
        println!("no updates");
    } else {
        println!("updates: {report}");
    }
    client.disconnect().await?;
    Ok(())
}
