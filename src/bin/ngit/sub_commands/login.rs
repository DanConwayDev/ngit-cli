use anyhow::{Context, Result};
use clap;

use crate::{
    cli::{extract_signer_cli_arguments, Cli},
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

pub async fn launch(args: &Cli, command_args: &SubCommandArgs) -> Result<()> {
    // TODO show existing login on record, prompt to logout

    let client = if command_args.offline {
        None
    } else {
        Some(Client::default())
    };

    let git_repo_result = Repo::discover().context("cannot find a git repository");
    let git_repo_option = {
        match git_repo_result {
            Ok(git_repo) => Some(git_repo),
            Err(_) => None,
        }
    };

    fresh_login_or_signup(
        &git_repo_option.as_ref(),
        client.as_ref(),
        extract_signer_cli_arguments(args)?,
        command_args.local,
    )
    .await?;

    // If not offline, disconnect the client
    if let Some(client) = client {
        client.disconnect().await?;
    }
    Ok(())
}
