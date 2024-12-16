use std::{path::Path, sync::Arc};

use anyhow::Result;
use fresh::fresh_login_or_signup;
use nostr::PublicKey;
use nostr_sdk::{NostrSigner, Timestamp, ToBech32};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::git::{Repo, RepoActions};

pub mod existing;
mod key_encryption;
use existing::load_existing_login;
pub mod user;
use user::UserRef;
pub mod fresh;

pub async fn login_or_signup(
    git_repo: &Option<&Repo>,
    signer_info: &Option<SignerInfo>,
    password: &Option<String>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    fetch_profile_updates: bool,
) -> Result<(Arc<dyn NostrSigner>, UserRef, SignerInfoSource)> {
    let res = load_existing_login(
        git_repo,
        signer_info,
        password,
        &None,
        client,
        false,
        true,
        fetch_profile_updates,
    )
    .await;
    if res.is_ok() {
        res
    } else {
        fresh_login_or_signup(git_repo, client, None, false).await
    }
}

#[derive(Clone)]
pub enum SignerInfo {
    Nsec {
        nsec: String,
        password: Option<String>,
        npub: Option<String>,
    },
    Bunker {
        bunker_uri: String,
        bunker_app_key: String,
        npub: Option<String>,
    },
}

#[derive(PartialEq, Clone)]
pub enum SignerInfoSource {
    GitLocal,
    GitGlobal,
    CommandLineArguments,
}

fn print_logged_in_as(
    user_ref: &UserRef,
    offline_mode: bool,
    source: &SignerInfoSource,
) -> Result<()> {
    if !offline_mode && user_ref.metadata.created_at.eq(&Timestamp::from(0)) {
        eprintln!("failed to find profile...");
    } else if !offline_mode && user_ref.metadata.name.eq(&user_ref.public_key.to_bech32()?) {
        eprintln!("failed to extract account name from account metadata...");
    } else if !offline_mode && user_ref.relays.created_at.eq(&Timestamp::from(0)) {
        eprintln!(
            "failed to find your relay list. consider using another nostr client to create one to enhance your nostr experience."
        );
    }
    eprintln!("logged in as {}{}", user_ref.metadata.name, match source {
        SignerInfoSource::CommandLineArguments => " via cli arguments",
        SignerInfoSource::GitLocal => " to local repository",
        SignerInfoSource::GitGlobal => "",
    });
    Ok(())
}

// None: in the edge case where the user is logged in via cli arguments rather
// than from git config this may be wrong. TODO: fix this
pub async fn get_likely_logged_in_user(git_repo_path: &Path) -> Result<Option<PublicKey>> {
    let git_repo = Repo::from_path(&git_repo_path.to_path_buf())?;
    Ok(
        if let Some(npub) = git_repo.get_git_config_item("nostr.npub", None)? {
            if let Ok(pubic_key) = PublicKey::parse(npub) {
                Some(pubic_key)
            } else {
                None
            }
        } else {
            None
        },
    )
}

pub fn get_curent_user(git_repo: &Repo) -> Result<Option<PublicKey>> {
    Ok(
        if let Some(npub) = git_repo.get_git_config_item("nostr.npub", None)? {
            if let Ok(public_key) = PublicKey::parse(npub) {
                Some(public_key)
            } else {
                None
            }
        } else {
            None
        },
    )
}
