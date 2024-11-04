#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

mod cli;
use ngit::{cli_interactor, client, git, git_events, login, repo_ref};

mod sub_commands;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Login(args) => sub_commands::login::launch(&cli, args).await,
        Commands::Init(args) => sub_commands::init::launch(&cli, args).await,
        Commands::Send(args) => sub_commands::send::launch(&cli, args, false).await,
        Commands::List => sub_commands::list::launch().await,
    }
}
