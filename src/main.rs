#![cfg_attr(not(test), warn(clippy::pedantic))]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod cli_interactor;
mod config;
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
    #[arg(short, long)]
    nsec: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// save encrypted nsec for future use
    Login(sub_commands::login::SubCommandArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Login(args) => sub_commands::login::launch(&cli, args),
    }
}
