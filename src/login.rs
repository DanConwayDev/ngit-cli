use std::str::FromStr;

use anyhow::{bail, Context, Result};
use nostr::PublicKey;
use zeroize::Zeroize;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptPasswordParms},
    config::{ConfigManagement, ConfigManager, UserRef},
    key_handling::{
        encryption::{EncryptDecrypt, Encryptor},
        users::{UserManagement, UserManager},
    },
};

/// handles the encrpytion and storage of key material
pub async fn launch(
    nsec: &Option<String>,
    password: &Option<String>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
) -> Result<(nostr::Keys, UserRef)> {
    // if nsec parameter
    let key = if let Some(nsec_unwrapped) = nsec {
        // get key or fail without prompts
        let key = nostr::Keys::from_str(nsec_unwrapped).context("invalid nsec parameter")?;

        // if password, add user to enable password login in future
        if password.is_some() {
            UserManager::default()
                .add(nsec, password)
                .context("could not store identity")?;
        } else {
            UserManager::default().add_user_to_config(key.public_key(), None, false)?;
        }
        key
    } else {
        let cfg = ConfigManager
            .load()
            .context("failed to load application config")?;
        // if encrypted nsec present
        if cfg.users.last().is_some() && !cfg.users.last().unwrap().encrypted_key.is_empty() {
            // unfortunately this line is unstable in rust:
            // if let Some(user) = cfg.users.last()  && !user.encrypted_key.is_empty() {
            let user = cfg.users.last().unwrap();
            let mut pass = if let Some(p) = password.clone() {
                p
            } else {
                println!("login as {}", &user.metadata.name);
                Interactor::default()
                    .password(PromptPasswordParms::default().with_prompt("password"))
                    .context("failed to get password input from interactor.password")?
            };

            let key_result = Encryptor
                .decrypt_key(&user.encrypted_key, pass.as_str())
                .context("failed to decrypt key with provided password");
            pass.zeroize();

            key_result.context(format!("failed to log in as {}", &user.metadata.name))?
        }
        // no encrypted nsec present
        else {
            // no nsec but password supplied
            if password.is_some() {
                bail!("no nsec available to decrypt with specified password");
            }
            // otherwise add new user with nsec and password prompts
            UserManager::default()
                .add(nsec, password)
                .context("failed to add user")?
        }
    };

    // get user details
    let user_ref = if let Some(client) = client {
        get_user_details(&key.public_key(), client).await?
    } else {
        // this will get user details with name as npub
        UserManager::default()
            .get_user_from_cache(&key.public_key())?
            .clone()
    };

    // print logged in
    println!("logged in as {}", user_ref.metadata.name);

    Ok((key, user_ref.clone()))
}

async fn get_user_details(
    public_key: &PublicKey,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Result<UserRef> {
    let term = console::Term::stdout();
    term.write_line("searching for profile and relay updates...")?;
    let user_manager = UserManager::default();
    let user_ref = user_manager
        .get_user(
            client,
            public_key,
            // use cache for 3 minutes
            3 * 60,
        )
        .await?;
    term.clear_last_lines(1)?;
    if user_ref.metadata.created_at.eq(&0) {
        println!("cannot find your account metadata (name, etc) on relays",);
        // TODO use secondary fallback list of relays.
        // TODO better reporting of what relays were checked and what the user
        //      here is a starter:
        //      cannot find account details on relays:
        //       - purplepages.xyz
        //       - fallbackrelay1
        //       - ...
        //      would you like to:
        //        [-] proceed anyway
        //         - add custom fallback relays
    } else if user_ref.relays.created_at.eq(&0) {
        println!(
            "cannot find your relay list. consider using another nostr client to create one to enhance your nostr experience."
        );
        // TODO better guidance on how to do this
    }
    Ok(user_ref)
}
