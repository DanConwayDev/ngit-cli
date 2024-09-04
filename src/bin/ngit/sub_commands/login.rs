use anyhow::{Context, Result};
use clap;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{cli::Cli, client::Connect, git::Repo, login};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// don't fetch user metadata and relay list from relays
    #[arg(long, action)]
    offline: bool,
}

pub async fn launch(args: &Cli, command_args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;
    if command_args.offline {
        login::launch(
            &git_repo,
            &args.bunker_uri,
            &args.bunker_app_key,
            &args.nsec,
            &args.password,
            None,
            true,
            false,
        )
        .await?;
        Ok(())
    } else {
        #[cfg(not(test))]
        let client = Client::default();
        #[cfg(test)]
        let client = <MockConnect as std::default::Default>::default();

        login::launch(
            &git_repo,
            &args.bunker_uri,
            &args.bunker_app_key,
            &args.nsec,
            &args.password,
            Some(&client),
            true,
            false,
        )
        .await?;
        client.disconnect().await?;
        Ok(())
    }
}
