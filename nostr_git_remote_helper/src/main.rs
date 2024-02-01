#![cfg_attr(not(test), warn(clippy::pedantic))]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod sub_commands;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// replace with an actual subcommand
    Placeholder(sub_commands::placeholder::SubCommandArgs),
    Capabilities(),
    // list
    //  - get git list from remote git server
    //  - suppliment list with open prs and send back
    //    - get prs
    //    - get commits against pr
    //    - find most recent commit against pr
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Capabilities() => sub_commands::capabilities::launch(),
        Commands::Placeholder(args) => {
            sub_commands::placeholder::launch(&cli, args).await
        }
    }
}
