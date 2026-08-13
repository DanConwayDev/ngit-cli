use anyhow::{Context, Result};
use ngit::{
    git::{get_git_config_item, remove_git_config_item},
    login::{SignerInfoSource, credential_store, existing::load_existing_login},
};
use nostr::prelude::ToBech32;

use crate::{
    git::Repo,
    sub_commands::login::{format_items_as_list, get_global_login_config_items_set},
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// also remove the account secret from the credential store
    #[arg(long)]
    pub forget: bool,
}

pub async fn launch(args: &SubCommandArgs) -> Result<()> {
    let git_repo_result = Repo::discover().context("failed to find a git repository");
    let git_repo = { git_repo_result.ok() };
    logout(git_repo.as_ref(), args.forget).await
}

const LOGIN_CONFIG_ITEMS: [&str; 5] = [
    "nostr.nsec",
    "nostr.npub",
    "nostr.bunker-uri",
    "nostr.bunker-app-key",
    "nostr.signer",
];

async fn logout(git_repo: Option<&Repo>, forget: bool) -> Result<()> {
    for source in if std::env::var("NGITTEST").is_ok() {
        vec![SignerInfoSource::GitLocal]
    } else {
        vec![SignerInfoSource::GitLocal, SignerInfoSource::GitGlobal]
    } {
        let scope = if source == SignerInfoSource::GitLocal {
            git_repo
        } else {
            None
        };
        if !has_login_config(scope) {
            continue;
        }
        let loaded = load_existing_login(
            &git_repo,
            &None,
            &None,
            &Some(source.clone()),
            None,
            true,
            false,
            false,
        )
        .await;
        let npub = loaded
            .as_ref()
            .ok()
            .and_then(|(_, user_ref, _)| user_ref.public_key.to_bech32().ok());
        let pointers = credential_store::config_pointers(&scope, npub.as_deref());
        if forget {
            forget_pointers(&pointers)?;
        }
        for item in LOGIN_CONFIG_ITEMS {
            if let Err(error) = remove_git_config_item(&scope, item) {
                if let Ok((_, user_ref, _)) = &loaded {
                    println!(
                        "failed to log out {}as {}",
                        if source == SignerInfoSource::GitLocal {
                            "from local git repository "
                        } else {
                            ""
                        },
                        user_ref.metadata.name
                    );
                }
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
                return Ok(());
            }
        }
        if let Ok((_, user_ref, _)) = loaded {
            println!(
                "logged out {}as {}",
                if source == SignerInfoSource::GitLocal {
                    "from local git repository "
                } else {
                    ""
                },
                user_ref.metadata.name
            );
        }
        hint_retained_secrets(forget, &pointers);
        return Ok(());
    }
    Ok(())
}

fn has_login_config(scope: Option<&Repo>) -> bool {
    [
        "nostr.nsec",
        "nostr.bunker-uri",
        "nostr.bunker-app-key",
        "nostr.signer",
    ]
    .iter()
    .any(|item| get_git_config_item(&scope, item).is_ok_and(|value| value.is_some()))
}

fn forget_pointers(pointers: &[String]) -> Result<()> {
    for pointer in pointers {
        credential_store::forget(pointer).with_context(|| {
            format!(
                "failed to remove credential entry {pointer}; remove it via your OS keychain UI or `ngit account forget-keys {pointer}`"
            )
        })?;
    }
    Ok(())
}

/// Logout deliberately keeps stored secrets: the credential store may hold
/// the only copy of an identity key, so deleting it on logout could destroy
/// the account. Point at the explicit removal command instead.
fn hint_retained_secrets(forget: bool, pointers: &[String]) {
    if forget {
        return;
    }
    for pointer in pointers {
        eprintln!(
            "the account secret remains in the credential store; remove it with: ngit account forget-keys {pointer}"
        );
    }
}
