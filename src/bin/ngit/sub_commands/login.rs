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
    let git_repo = Repo::discover().context("cannot find a git repository")?;
    // TODO show existing login on record, prompt to logout
    // TODO use cli arguments to login
    if command_args.offline {
        fresh_login_or_signup(&Some(&git_repo), None, command_args.local).await?;
        Ok(())
    } else {
        let client = Client::default();
        fresh_login_or_signup(&Some(&git_repo), Some(&client), command_args.local).await?;

        client.disconnect().await?;
        Ok(())
    }
}
