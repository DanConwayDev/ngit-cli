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
    /// issue a repo event as a maintainers to signal you are accepting
    /// proposals via nostr
    Init(sub_commands::init::SubCommandArgs),
    /// issue commits on current branch as a new proposal
    Send(sub_commands::send::SubCommandArgs),
    /// list proposals; optionally apply them as a new branch
    List(sub_commands::list::SubCommandArgs),
    /// send new commits as proposal amendments
    Push,
    /// pull latest commits in proposal linked to checked out branch
    Pull,
    /// run with --nsec flag to change npub
    Login(sub_commands::login::SubCommandArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Login(args) => sub_commands::login::launch(&cli, args).await,
        Commands::Init(args) => sub_commands::init::launch(&cli, args).await,
        Commands::Send(args) => sub_commands::send::launch(&cli, args).await,
        Commands::List(args) => sub_commands::list::launch(&cli, args).await,
        Commands::Pull => sub_commands::pull::launch().await,
        Commands::Push => sub_commands::push::launch(&cli).await,
    }
}
