use anyhow::Result;
use clap::Subcommand;

use crate::Cli;
pub mod create;
pub mod list;

#[derive(clap::Parser)]
pub struct SubCommandArgs {
    #[command(subcommand)]
    pub prs_command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Create(create::SubCommandArgs),
    List(list::SubCommandArgs),
}

pub async fn launch(cli_args: &Cli, pr_args: &SubCommandArgs) -> Result<()> {
    match &pr_args.prs_command {
        Commands::Create(args) => create::launch(cli_args, pr_args, args).await,
        Commands::List(args) => list::launch(cli_args, pr_args, args).await,
    }
}
