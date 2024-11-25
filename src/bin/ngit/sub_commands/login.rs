use anyhow::{Context, Result};
use clap;
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    git::remove_git_config_item,
    login::{existing::load_existing_login, SignerInfoSource},
};

use crate::{
    cli::{extract_signer_cli_arguments, Cli},
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
    // TODO show existing login on record, prompt to logout

    let client = if command_args.offline {
        None
    } else {
        Some(Client::default())
    };

    let git_repo_result = Repo::discover().context("cannot find a git repository");
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
            command_args.local,
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
    for source in if local_only {
        vec![SignerInfoSource::GitLocal]
    } else {
        vec![SignerInfoSource::GitLocal, SignerInfoSource::GitGlobal]
    } {
        if let Ok((_, user_ref, source)) =
            load_existing_login(&git_repo, &None, &None, &Some(source), None, true, false).await
        {
            eprintln!(
                "logged in {}as {}",
                if source == SignerInfoSource::GitLocal {
                    "to local git repository "
                } else {
                    ""
                },
                user_ref.metadata.name
            );
            match Interactor::default().choice(
                PromptChoiceParms::default().with_default(0).with_choices(
                    if source == SignerInfoSource::GitGlobal {
                        vec![
                            format!("logout as \"{}\"", user_ref.metadata.name),
                            "remain logged in".to_string(),
                            "login to local git repo only as another user".to_string(),
                        ]
                    } else {
                        vec![
                            format!("logout as \"{}\"", user_ref.metadata.name),
                            "remain logged in".to_string(),
                        ]
                    },
                ),
            )? {
                0 => {
                    for item in [
                        "nostr.nsec",
                        "nostr.npub",
                        "nostr.bunker-uri",
                        "nostr.bunker-app-key",
                    ] {
                        remove_git_config_item(
                            if source == SignerInfoSource::GitLocal {
                                &git_repo
                            } else {
                                &None
                            },
                            item,
                        )?;
                    }
                }
                1 => return Ok((false, local_only)),
                _ => return Ok((false, true)),
            }
        }
    }
    Ok((true, local_only))
}
