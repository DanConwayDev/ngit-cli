#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use clap::Parser;
use cli::{AccountCommands, CUSTOMISE_TEMPLATE, Cli, Commands, IssueCommands, PrCommands};

mod cli;
use ngit::{
    cli_interactor::{self, CliError},
    client,
    git::{self, utils::set_git_timeout},
    git_events, login, repo_ref,
};

mod sub_commands;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() {
    let cli = Cli::parse();

    // Non-interactive by default; set NGIT_INTERACTIVE_MODE only when -i is
    // specified
    if cli.interactive {
        std::env::set_var("NGIT_INTERACTIVE_MODE", "1");
    }

    if cli.verbose || std::env::var("NGITTEST").is_ok() {
        std::env::set_var("NGIT_VERBOSE", "1");
    }

    if cli.customize {
        print!("{CUSTOMISE_TEMPLATE}");
        std::process::exit(0); // Exit the program
    }

    let _ = set_git_timeout();

    let result = if let Some(command) = &cli.command {
        match command {
            Commands::Account(args) => match &args.account_command {
                AccountCommands::Login(sub_args) => {
                    sub_commands::login::launch(&cli, sub_args).await
                }
                AccountCommands::Logout => sub_commands::logout::launch().await,
                AccountCommands::ExportKeys => sub_commands::export_keys::launch().await,
                AccountCommands::Create(sub_args) => {
                    sub_commands::create::launch(&cli, sub_args).await
                }
            },
            Commands::Init(args) => sub_commands::init::launch(&cli, args).await,
            Commands::Repo(args) => {
                sub_commands::repo::launch(&cli, args.repo_command.as_ref(), args.offline).await
            }
            Commands::Send(args) => sub_commands::send::launch(&cli, args, false).await,
            Commands::Pr(args) => match &args.pr_command {
                PrCommands::List {
                    status,
                    json,
                    id,
                    offline,
                } => sub_commands::list::launch(status.clone(), *json, id.clone(), *offline).await,
                PrCommands::View { id, json, offline } => {
                    sub_commands::list::launch(
                        "open,draft,closed,applied".to_string(),
                        *json,
                        Some(id.clone()),
                        *offline,
                    )
                    .await
                }
                PrCommands::Checkout { id, offline } => {
                    sub_commands::checkout::launch(id, *offline).await
                }
                PrCommands::Apply {
                    id,
                    stdout,
                    offline,
                } => sub_commands::apply::launch(id, *stdout, *offline).await,
                PrCommands::Send(sub_args) => {
                    sub_commands::send::launch(&cli, sub_args, false).await
                }
                PrCommands::Close { id, offline } => {
                    sub_commands::pr_status::launch_close(id, *offline).await
                }
                PrCommands::Reopen { id, offline } => {
                    sub_commands::pr_status::launch_reopen(id, *offline).await
                }
                PrCommands::Ready { id, offline } => {
                    sub_commands::pr_status::launch_ready(id, *offline).await
                }
                PrCommands::Comment {
                    id,
                    body,
                    reply_to,
                    offline,
                } => {
                    sub_commands::comment::launch_pr_comment(
                        id,
                        body,
                        reply_to.as_deref(),
                        *offline,
                    )
                    .await
                }
                PrCommands::Merge {
                    id,
                    squash,
                    offline,
                } => sub_commands::pr_merge::launch(id, *squash, *offline).await,
            },
            Commands::Issue(args) => match &args.issue_command {
                IssueCommands::List {
                    status,
                    hashtag,
                    json,
                    id,
                    offline,
                } => {
                    sub_commands::issue_list::launch(
                        status.clone(),
                        hashtag.clone(),
                        *json,
                        id.clone(),
                        *offline,
                    )
                    .await
                }
                IssueCommands::View { id, json, offline } => {
                    sub_commands::issue_list::launch(
                        "open,draft,closed,applied".to_string(),
                        None,
                        *json,
                        Some(id.clone()),
                        *offline,
                    )
                    .await
                }
                IssueCommands::Create {
                    title,
                    body,
                    labels,
                } => {
                    sub_commands::issue_create::launch(title.clone(), body.clone(), labels.clone())
                        .await
                }
                IssueCommands::Close { id, offline } => {
                    sub_commands::issue_status::launch_close(id, *offline).await
                }
                IssueCommands::Reopen { id, offline } => {
                    sub_commands::issue_status::launch_reopen(id, *offline).await
                }
                IssueCommands::Comment {
                    id,
                    body,
                    reply_to,
                    offline,
                } => {
                    sub_commands::comment::launch_issue_comment(
                        id,
                        body,
                        reply_to.as_deref(),
                        *offline,
                    )
                    .await
                }
            },
            Commands::Sync(args) => sub_commands::sync::launch(args).await,
        }
    } else {
        // Show help when no command is provided
        Cli::parse_from(["ngit", "--help"]);
        std::process::exit(0);
    };

    if let Err(err) = result {
        if err.downcast_ref::<CliError>().is_some() {
            // Already printed styled output to stderr
            std::process::exit(1);
        }
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
