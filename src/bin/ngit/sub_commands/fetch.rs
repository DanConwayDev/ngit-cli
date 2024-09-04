use std::collections::HashSet;

use anyhow::{Context, Result};
use clap;
use nostr::nips::nip01::Coordinate;

use crate::{
    cli::Cli,
    client::{fetching_with_report, Client, Connect},
    git::{Repo, RepoActions},
    repo_ref::get_repo_coordinates,
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
    let client = Client::default();
    let repo_coordinates = if command_args.repo.is_empty() {
        get_repo_coordinates(&git_repo, &client).await?
    } else {
        let mut repo_coordinates = HashSet::new();
        for repo in &command_args.repo {
            repo_coordinates.insert(Coordinate::parse(repo.clone())?);
        }
        repo_coordinates
    };
    fetching_with_report(git_repo.get_path()?, &client, &repo_coordinates).await?;
    client.disconnect().await?;
    Ok(())
}
