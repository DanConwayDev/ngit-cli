#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use std::ffi::OsStr;

use clap::Parser;
use cli::{
    AccountCommands, CiCommands, Cli, Commands, IssueCommands, PrCommands, SignerParams,
    customise_template, extract_signer_cli_arguments,
};

mod cli;
use ngit::{
    cli_interactor::{self, CliError},
    client,
    git::{self, RepoActions, utils::set_git_timeout},
    git_events, login, repo_ref,
};

mod ci_commit;
mod docs_export;
mod git_remote_helper;
#[macro_use]
mod output;
// Declared after `output` so the stdout-guarding `println!` macro is in
// scope for it.
mod ci_projection;
mod push_bookkeeping;
mod state_transaction;
mod sub_commands;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() {
    // Documentation builds need the command model without touching a git
    // repository, cache, signer, or relay. Keep this raw internal dispatch
    // ahead of every startup side effect, just like the remote helper.
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(docs_export::INTERNAL_COMMAND)) {
        if let Err(err) = docs_export::write_stdout() {
            eprintln!("Error: {err:?}");
            std::process::exit(1);
        }
        return;
    }

    // Reqwest is intentionally compiled without its own provider. Select Ring
    // before any network-capable command path can construct an HTTP or relay
    // TLS client.
    ngit::tls::install_default_crypto_provider();

    // The remote-helper entry point must dispatch before anything that
    // could write to stdout (update notices, skill notices, clap
    // output): stray stdout would corrupt git's remote-helper protocol.
    // The token check uses args_os so a non-unicode argument still
    // reaches clap's graceful error path instead of panicking here.
    if std::env::args_os().nth(1).as_deref()
        == Some(OsStr::new(git_remote_helper::INTERNAL_COMMAND))
    {
        let helper_args: Vec<String> = std::env::args().skip(2).collect();
        if let Err(err) = git_remote_helper::run(&helper_args).await {
            // Match the exit behavior of the pre-consolidation
            // `git-remote-nostr` binary, whose `main` returned a
            // `Result` (std prints `Error: {err:?}`, exit code 1).
            eprintln!("Error: {err:?}");
            std::process::exit(1);
        }
        return;
    }

    if version_flag_requested() {
        print_update_notice_if_available_at_startup().await;
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return;
    }

    let cli = Cli::parse();
    output::set_json_mode(cli.json);

    // Non-interactive by default; set NGIT_INTERACTIVE_MODE only when -i is
    // specified
    if cli.interactive {
        std::env::set_var("NGIT_INTERACTIVE_MODE", "1");
    }

    if cli.verbose || std::env::var("NGITTEST").is_ok() {
        std::env::set_var("NGIT_VERBOSE", "1");
    }

    if cli.repo_relay_only {
        std::env::set_var("NGIT_REPO_RELAY_ONLY", "1");
    }

    if let Some(repo) = cli.repo.as_deref() {
        // Passed to `repo_ref::resolve_repo_coordinate` via env var so it
        // does not need threading through every subcommand.
        std::env::set_var(repo_ref::NGIT_REPO_ENV, repo);
    }

    if cli.customize {
        if cli.json {
            output::set_value(serde_json::json!({ "configuration": customise_template() }));
            output::finish_success();
        } else {
            print!("{}", customise_template());
        }
        std::process::exit(0); // Exit the program
    }

    print_update_notice_if_available_at_startup().await;
    if !matches!(cli.command, Some(Commands::Init(_) | Commands::Skill(_))) {
        print_skill_notice_if_available().await;
    }

    // Resolve an explicitly supplied signer once, before dispatch. This makes
    // invalid signer files fail closed instead of allowing an individual
    // command to fall back to a configured account.
    let signer_info = match extract_signer_cli_arguments(&cli) {
        Ok(signer_info) => signer_info,
        Err(err) => {
            if cli.json {
                output::finish_error(&err);
            }
            eprintln!("Error: {err:?}");
            std::process::exit(1);
        }
    };
    let signer_params = SignerParams {
        info: &signer_info,
        password: &cli.password,
    };

    let result = if let Some(command) = &cli.command {
        match command {
            Commands::Account(args) => match &args.account_command {
                AccountCommands::Whoami(sub_args) => {
                    sub_commands::whoami::launch(sub_args, cli.json).await
                }
                AccountCommands::Login(sub_args) => {
                    sub_commands::login::launch(sub_args, signer_params).await
                }
                AccountCommands::Connect(sub_args) => {
                    // `connect` is an alias for `login -i`: always interactive
                    std::env::set_var("NGIT_INTERACTIVE_MODE", "1");
                    sub_commands::login::launch(sub_args, signer_params).await
                }
                AccountCommands::Logout(sub_args) => sub_commands::logout::launch(sub_args).await,
                AccountCommands::ExportKeys => {
                    sub_commands::export_keys::launch(signer_params).await
                }
                AccountCommands::ForgetKeys(sub_args) => {
                    sub_commands::forget_keys::launch(sub_args)
                }
                AccountCommands::Create(sub_args) => {
                    sub_commands::create::launch(&cli, sub_args).await
                }
            },
            Commands::Init(args) => sub_commands::init::launch(&cli, args, signer_params).await,
            Commands::Repo(args) => {
                sub_commands::repo::launch(
                    &cli,
                    args.repo_command.as_ref(),
                    args.offline,
                    cli.json,
                    signer_params,
                )
                .await
            }
            Commands::Send(args) => {
                sub_commands::send::launch(&cli, args, false, signer_params).await
            }
            Commands::Pr(args) => match &args.pr_command {
                PrCommands::List {
                    status,
                    labels,
                    id,
                    offline,
                } => {
                    sub_commands::list::launch(
                        status.clone(),
                        labels.clone(),
                        cli.json,
                        false,
                        id.clone(),
                        *offline,
                        signer_params,
                    )
                    .await
                }
                PrCommands::View {
                    id,
                    comments,
                    offline,
                } => {
                    sub_commands::list::launch(
                        "open,draft,closed,applied".to_string(),
                        vec![],
                        cli.json,
                        *comments,
                        Some(id.clone()),
                        *offline,
                        signer_params,
                    )
                    .await
                }
                PrCommands::Checkout { id, force, offline } => {
                    sub_commands::checkout::launch(id, *force, *offline, signer_params).await
                }
                PrCommands::Apply {
                    id,
                    stdout,
                    offline,
                } => sub_commands::apply::launch(id, *stdout, *offline, signer_params).await,
                PrCommands::Send(sub_args) => {
                    sub_commands::send::launch(&cli, sub_args, false, signer_params).await
                }
                PrCommands::Close {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::pr_status::launch_close(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
                }
                PrCommands::Reopen {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::pr_status::launch_reopen(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
                }
                PrCommands::Ready {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::pr_status::launch_ready(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
                }
                PrCommands::Draft {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::pr_status::launch_draft(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
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
                        signer_params,
                    )
                    .await
                }
                PrCommands::Merge {
                    id,
                    squash,
                    require_ci_trust,
                    offline,
                } => {
                    sub_commands::pr_merge::launch(
                        id,
                        *squash,
                        *require_ci_trust,
                        *offline,
                        signer_params,
                    )
                    .await
                }
                PrCommands::Label {
                    id,
                    labels,
                    offline,
                } => {
                    sub_commands::label::launch_pr_label(id, labels, *offline, signer_params).await
                }
                PrCommands::SetSubject {
                    id,
                    subject,
                    offline,
                } => {
                    sub_commands::set_subject::launch_pr_set_subject(
                        id,
                        subject,
                        *offline,
                        signer_params,
                    )
                    .await
                }
                PrCommands::SetCoverNote { id, body, offline } => {
                    sub_commands::set_cover_note::launch_pr_set_cover_note(
                        id,
                        body,
                        *offline,
                        signer_params,
                    )
                    .await
                }
            },
            Commands::Issue(args) => match &args.issue_command {
                IssueCommands::List {
                    status,
                    labels,
                    comments,
                    id,
                    offline,
                } => {
                    sub_commands::issue_list::launch(
                        status.clone(),
                        labels.clone(),
                        cli.json,
                        *comments,
                        id.clone(),
                        *offline,
                        signer_params,
                    )
                    .await
                }
                IssueCommands::View {
                    id,
                    comments,
                    offline,
                } => {
                    sub_commands::issue_list::launch(
                        "open,draft,closed,applied".to_string(),
                        vec![],
                        cli.json,
                        *comments,
                        Some(id.clone()),
                        *offline,
                        signer_params,
                    )
                    .await
                }
                IssueCommands::Create {
                    subject,
                    body,
                    labels,
                } => {
                    sub_commands::issue_create::launch(
                        subject.clone(),
                        body.clone(),
                        labels.clone(),
                        signer_params,
                    )
                    .await
                }
                IssueCommands::Close {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::issue_status::launch_close(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
                }
                IssueCommands::Resolved {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::issue_status::launch_resolved(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
                }
                IssueCommands::Reopen {
                    id,
                    reason,
                    offline,
                } => {
                    sub_commands::issue_status::launch_reopen(
                        id,
                        *offline,
                        reason.as_deref(),
                        signer_params,
                    )
                    .await
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
                        signer_params,
                    )
                    .await
                }
                IssueCommands::Label {
                    id,
                    labels,
                    offline,
                } => {
                    sub_commands::label::launch_issue_label(id, labels, *offline, signer_params)
                        .await
                }
                IssueCommands::SetSubject {
                    id,
                    subject,
                    offline,
                } => {
                    sub_commands::set_subject::launch_issue_set_subject(
                        id,
                        subject,
                        *offline,
                        signer_params,
                    )
                    .await
                }
                IssueCommands::SetCoverNote { id, body, offline } => {
                    sub_commands::set_cover_note::launch_issue_set_cover_note(
                        id,
                        body,
                        *offline,
                        signer_params,
                    )
                    .await
                }
            },
            Commands::Ci(args) => match &args.ci_command {
                CiCommands::Status {
                    target,
                    require_ci_trust,
                    offline,
                } => {
                    sub_commands::ci_status::launch(
                        target.as_deref(),
                        *offline,
                        *require_ci_trust,
                        cli.json,
                        signer_params,
                    )
                    .await
                }
                CiCommands::Request {
                    coordinator,
                    offline,
                } => {
                    sub_commands::ci_control::launch_request(coordinator, *offline, signer_params)
                        .await
                }
                CiCommands::Stop {
                    coordinator,
                    offline,
                } => {
                    sub_commands::ci_control::launch_stop(coordinator, *offline, signer_params)
                        .await
                }
                CiCommands::Trigger {
                    coordinator,
                    commit_ish,
                    workflow,
                    git_ref,
                    offline,
                } => {
                    sub_commands::ci_control::launch_trigger(
                        coordinator,
                        commit_ish.as_deref(),
                        workflow,
                        git_ref.as_deref(),
                        *offline,
                        signer_params,
                    )
                    .await
                }
            },
            Commands::Release(args) => {
                sub_commands::release::launch(args, signer_params, cli.json).await
            }
            Commands::Sync(args) => sub_commands::sync::launch(args, signer_params).await,
            Commands::Skill(args) => {
                sub_commands::skill::launch(&args.skill_command, cli.force, cli.json, signer_params)
                    .await
            }
            Commands::Merge(args) => {
                sub_commands::merge::launch(
                    args.id.as_deref(),
                    args.offline,
                    args.exclude_description,
                    signer_params,
                )
                .await
            }
        }
    } else {
        // Show help when no command is provided
        Cli::parse_from(["ngit", "--help"]);
        std::process::exit(0);
    };

    if let Err(err) = result {
        if cli.json {
            output::finish_error(&err);
        }
        if err.downcast_ref::<CliError>().is_some() {
            // Already printed styled output to stderr
            std::process::exit(1);
        }
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
    if cli.json {
        output::finish_success();
    }
}

fn version_flag_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .any(|arg| arg == OsStr::new("--version") || arg == OsStr::new("-V"))
}

async fn print_update_notice_if_available_at_startup() {
    let git_repo = git::Repo::discover().ok();
    let _ = set_git_timeout(git_repo.as_ref());
    let git_repo_path = git_repo.as_ref().and_then(|repo| repo.get_path().ok());
    let _ = ngit::version_check::print_update_notice_if_available(git_repo_path).await;
}

async fn print_skill_notice_if_available() {
    let Ok(repo) = git::Repo::discover() else {
        return;
    };
    let Ok(root) = repo.get_path() else {
        return;
    };
    let Ok(Some((_, remote))) = repo.get_first_nostr_remote_when_in_ngit_binary().await else {
        return;
    };
    let Ok(repo_ref) = client::get_repo_ref_from_cache(Some(root), &remote.coordinate).await else {
        return;
    };
    let _ = ngit::agent_guidance::warn_if_maintainer(&repo, &repo_ref).await;
}
