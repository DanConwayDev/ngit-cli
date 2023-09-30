#![cfg_attr(not(test), warn(clippy::pedantic))]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod cli_interactor;
mod config;
mod git;
mod key_handling;
mod login;
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
}

#[derive(Subcommand)]
enum Commands {
    /// save encrypted nsec for future use
    Login(sub_commands::login::SubCommandArgs),
    /// create and issue Prs
    Prs(sub_commands::prs::SubCommandArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Login(args) => sub_commands::login::launch(&cli, args),
        Commands::Prs(args) => sub_commands::prs::launch(&cli, args),
    }
}
