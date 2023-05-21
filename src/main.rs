use clap::{Parser, Subcommand};
use nostr_sdk::Result;

mod branch_refs;
mod sub_commands;
mod funcs;
mod fetch_pull_push;
mod groups;
mod merge;
mod pull_request;
mod repos;
mod patch;
mod ngit_tag;
mod kind;
mod utils;
mod config;
mod repo_config;
mod cli_helpers;

/// Simple CLI application to use git through nostr
#[derive(Parser)]
#[command(name = "ngit")]
#[command(author = "DanConwayDev <DanConwayDev@protonmail.com")]
#[command(version = "0.0.1")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Relay to connect to
    #[arg(short, long, action = clap::ArgAction::Append)]
    relays: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a repoistory
    Clone(sub_commands::clone::CloneSubCommand),
    /// Initialize a repoistory
    Init(sub_commands::init::InitSubCommand),
    /// Pull to events and relays
    Pull(sub_commands::pull::PullSubCommand),
    /// Push to events and relays
    Push(sub_commands::push::PushSubCommand),
    /// Merge to events and relays
    Merge(sub_commands::merge::MergeSubCommand),
    /// Fetch from relays
    Fetch(sub_commands::fetch::FetchSubCommand),
    /// View active PRs from relays
    Prs(sub_commands::prs::PrsSubCommand),
    /// rebroadcast all repository events
    Rebroadcast(sub_commands::rebroadcast::RebroadcastSubCommand),
    ChangeUser(sub_commands::change_user::ChangeUserSubCommand),
}

fn main() -> Result<()> {
    println!("ngit prototype v0.0.1-alpha");
    // Parse input
    let args: Cli = Cli::parse();

    // Post event
    match &args.command {
        Commands::Init(sub_command_args) => sub_commands::init::create_and_broadcast_init(
            args.relays,
            sub_command_args,
        ),
        Commands::Clone(sub_command_args) => {
            sub_commands::clone::clone_from_relays(
                args.relays,
                sub_command_args,
            );
            Ok(())
        },
        Commands::Pull(sub_command_args) => {
            sub_commands::pull::pull_from_relays(
                None,
                sub_command_args,
            );
            Ok(())
        },
        Commands::Push(sub_command_args) => {
            sub_commands::push::push(
                sub_command_args,
            );
            Ok(())
        }
        Commands::Merge(sub_command_args) => {
            sub_commands::merge::merge(
                sub_command_args,
            );
            Ok(())
        }
        Commands::Fetch(_sub_command_args) => {
            sub_commands::fetch::fetch_from_relays(None);
            Ok(())
        },
        Commands::Prs(sub_command_args) => {
            sub_commands::prs::prs(
                sub_command_args,
            );
            Ok(())
        }
        Commands::Rebroadcast(sub_command_args) => {
            sub_commands::rebroadcast::rebroadcast(
                sub_command_args,
            );
            Ok(())
        }
        Commands::ChangeUser(sub_command_args) => {
            sub_commands::change_user::change_user(
                sub_command_args,
            );
            Ok(())
        }
    }
}
