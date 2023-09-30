use anyhow::Result;
use clap::Subcommand;

use crate::Cli;
pub mod create;

#[derive(clap::Parser)]
pub struct SubCommandArgs {
    #[command(subcommand)]
    pub prs_command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Create(create::SubCommandArgs),
}

pub fn launch(cli_args: &Cli, pr_args: &SubCommandArgs) -> Result<()> {
    match &pr_args.prs_command {
        Commands::Create(args) => create::launch(cli_args, pr_args, args),
    }
}
