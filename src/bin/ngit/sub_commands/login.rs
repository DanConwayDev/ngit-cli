use anyhow::{Context, Result};
use clap;
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    git::{get_git_config_item, remove_git_config_item},
    login::{SignerInfoSource, existing::load_existing_login},
};

use crate::{
    cli::{Cli, extract_signer_cli_arguments},
    client::{Client, Connect},
    git::Repo,
    login::fresh::fresh_login_or_signup,
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// login to the local git repository only
    #[arg(long, action)]
    local: bool,

    /// don't fetch user metadata and relay list from relays
    #[arg(long, action)]
    offline: bool,
}

pub async fn launch(args: &Cli, command_args: &SubCommandArgs) -> Result<()> {
    let client = if command_args.offline {
        None
    } else {
        Some(Client::default())
    };

    let git_repo_result = Repo::discover().context("failed to find a git repository");
    let git_repo = {
        match git_repo_result {
            Ok(git_repo) => Some(git_repo),
            Err(_) => None,
        }
    };

    let (logged_out, log_in_locally_only) = logout(git_repo.as_ref(), command_args.local).await?;
    if logged_out || log_in_locally_only {
        fresh_login_or_signup(
            &git_repo.as_ref(),
            client.as_ref(),
            extract_signer_cli_arguments(args)?,
            log_in_locally_only || command_args.local,
        )
        .await?;
    }

    // If not offline, disconnect the client
    if let Some(client) = client {
        client.disconnect().await?;
    }
    Ok(())
}

/// return ( bool - logged out, bool - log in to local git locally)
async fn logout(git_repo: Option<&Repo>, local_only: bool) -> Result<(bool, bool)> {
    for source in if local_only || std::env::var("NGITTEST").is_ok() {
        vec![SignerInfoSource::GitLocal]
    } else {
        vec![SignerInfoSource::GitLocal, SignerInfoSource::GitGlobal]
    } {
        if let Ok((_, user_ref, source)) = load_existing_login(
            &git_repo,
            &None,
            &None,
            &Some(source),
            None,
            true,
            false,
            false,
        )
        .await
        {
            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_prompt(format!(
                        "logged in {}as {}",
                        if source == SignerInfoSource::GitLocal {
                            "to local git repository "
                        } else {
                            ""
                        },
                        user_ref.metadata.name
                    ))
                    .with_choices(if source == SignerInfoSource::GitGlobal {
                        vec![
                            "logout".to_string(),
                            "remain logged in".to_string(),
                            "login to local git repo only as another user".to_string(),
                        ]
                    } else {
                        vec![
                            format!("logout as \"{}\"", user_ref.metadata.name),
                            "remain logged in".to_string(),
                        ]
                    }),
            )? {
                0 => {
                    for item in [
                        "nostr.nsec",
                        "nostr.npub",
                        "nostr.bunker-uri",
                        "nostr.bunker-app-key",
                    ] {
                        if let Err(error) = remove_git_config_item(
                            if source == SignerInfoSource::GitLocal {
                                &git_repo
                            } else {
                                &None
                            },
                            item,
                        ) {
                            eprintln!("{error:?}");

                            eprintln!(
                                "consider manually removing {} git config items: {}",
                                if source == SignerInfoSource::GitGlobal {
                                    "global"
                                } else {
                                    "local"
                                },
                                format_items_as_list(&get_global_login_config_items_set())
                            );
                            match Interactor::default().choice(
                                PromptChoiceParms::default().with_default(0)
                                .with_prompt("failed to remove login details from global git config")
                                .with_choices(
                                    vec![
                                        "continue with global login to reveal what git config items to manually set".to_string(),
                                        "login to this local repository with a different account".to_string(),
                                        "cancel".to_string(),
                                    ]
                                ),
                            )? {
                                0 => return Ok((true, false)),
                                1 => return Ok((true, true)),
                                _ => return Ok((false, local_only)),
                            }
                        }
                    }
                }
                1 => return Ok((false, local_only)),
                _ => return Ok((false, true)),
            }
        }
    }
    Ok((true, local_only))
}

pub fn get_global_login_config_items_set() -> Vec<&'static str> {
    [
        "nostr.nsec",
        "nostr.npub",
        "nostr.bunker-uri",
        "nostr.bunker-app-key",
    ]
    .iter()
    .copied()
    .filter(|item| get_git_config_item(&None, item).is_ok_and(|item| item.is_some()))
    .collect::<Vec<&str>>()
}

pub fn format_items_as_list(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let all_but_last = items[..items.len() - 1].join(", ");
            format!("{}, and {}", all_but_last, items[items.len() - 1])
        }
    }
}
