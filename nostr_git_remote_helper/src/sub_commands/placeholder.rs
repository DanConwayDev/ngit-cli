use anyhow::Result;

use crate::Cli;

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {}

pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    Ok(())
}
