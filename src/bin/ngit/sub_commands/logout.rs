use anyhow::{Context, Result};
use ngit::{
    git::{get_git_config_item, remove_git_config_item},
    login::{SignerInfoSource, credential_store, existing::load_existing_login},
};

use crate::{
    git::Repo,
    sub_commands::login::{format_items_as_list, get_global_login_config_items_set},
};

pub async fn launch() -> Result<()> {
    let git_repo_result = Repo::discover().context("failed to find a git repository");
    let git_repo = { git_repo_result.ok() };
    logout(git_repo.as_ref()).await
}

async fn logout(git_repo: Option<&Repo>) -> Result<()> {
    for source in if std::env::var("NGITTEST").is_ok() {
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
            credential_store::delete_config_pointers(&if source == SignerInfoSource::GitLocal {
                git_repo
            } else {
                None
            })?;
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
                    println!(
                        "failed to log out {}as {}",
                        if source == SignerInfoSource::GitLocal {
                            "from local git repository "
                        } else {
                            ""
                        },
                        user_ref.metadata.name
                    );
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
            println!(
                "logged out {}as {}",
                if source == SignerInfoSource::GitLocal {
                    "from local git repository "
                } else {
                    ""
                },
                user_ref.metadata.name
            );
            return Ok(());
        }
    }
    // A dangling pointer cannot be loaded as a signer, but logout must still
    // clear it so the user can recover with a fresh login.
    // NGITTEST limits the sweep to local config, mirroring the login flow, so
    // tests never touch the developer's real global git config.
    for scope in if std::env::var("NGITTEST").is_ok() {
        vec![git_repo]
    } else {
        vec![git_repo, None]
    } {
        let has_login = ["nostr.nsec", "nostr.bunker-uri", "nostr.bunker-app-key"]
            .iter()
            .any(|item| get_git_config_item(&scope, item).is_ok_and(|value| value.is_some()));
        if has_login {
            credential_store::delete_config_pointers(&scope)?;
            for item in [
                "nostr.nsec",
                "nostr.npub",
                "nostr.bunker-uri",
                "nostr.bunker-app-key",
            ] {
                remove_git_config_item(&scope, item)?;
            }
            return Ok(());
        }
    }
    Ok(())
}
