use anyhow::{Context, Result};
use clap;

use crate::{
    cli::Cli,
    client::{Client, Connect},
    git::Repo,
    login::fresh::fresh_login_or_signup,
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// login to the local git repository only
    #[arg(long, action)]
    local: bool,

    /// don't fetch user metadata and relay list from relays
    #[arg(long, action)]
    offline: bool,
}

pub async fn launch(_args: &Cli, command_args: &SubCommandArgs) -> Result<()> {
    // TODO show existing login on record, prompt to logout
    // TODO use cli arguments to login
    let git_repo_result = Repo::discover().context("cannot find a git repository");

    if command_args.offline {
        if let Ok(git_repo) = git_repo_result {
            fresh_login_or_signup(&Some(&git_repo), None, command_args.local).await?;
        } else {
            fresh_login_or_signup(&None, None, command_args.local).await?;
        }
    } else {
        let client = Client::default();
        if let Ok(git_repo) = git_repo_result {
            fresh_login_or_signup(&Some(&git_repo), Some(&client), command_args.local).await?;
        } else {
            fresh_login_or_signup(&None, Some(&client), command_args.local).await?;
        }
        client.disconnect().await?;
    }
    Ok(())
}
