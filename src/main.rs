#![cfg_attr(not(test), warn(clippy::pedantic))]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod cli_interactor;
mod client;
mod config;
mod git;
mod key_handling;
mod login;
mod repo_ref;
mod sub_commands;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// nsec or hex private key
    #[arg(short, long, global = true)]
    nsec: Option<String>,
    /// password to decrypt nsec
    #[arg(short, long, global = true)]
    password: Option<String>,
    /// disable spinner animations
    #[arg(long, action)]
    disable_cli_spinners: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// save encrypted nsec for future use
    Login(sub_commands::login::SubCommandArgs),
    /// issue repository reference event as a maintainers
    Claim(sub_commands::claim::SubCommandArgs),
    /// send a PR / patch / patch set via nostr events
    Send(sub_commands::send::SubCommandArgs),
    /// list open PRs / patches / patch sets and pull / apply them a branch
    List(sub_commands::list::SubCommandArgs),
    /// pull latest commits in pr linked to checked out branch
    Pull,
    /// push commits to current checked out pr branch
    Push,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Login(args) => sub_commands::login::launch(&cli, args).await,
        Commands::Claim(args) => sub_commands::claim::launch(&cli, args).await,
        Commands::Send(args) => sub_commands::send::launch(&cli, args).await,
        Commands::List(args) => sub_commands::list::launch(&cli, args).await,
        Commands::Pull => sub_commands::pull::launch().await,
        Commands::Push => sub_commands::push::launch(&cli).await,
    }
}
