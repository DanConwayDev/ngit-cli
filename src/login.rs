use anyhow::{bail, Context, Result};
use nostr::prelude::{FromSkStr, ToBech32};
use zeroize::Zeroize;

use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptPasswordParms},
    config::{ConfigManagement, ConfigManager},
    key_handling::{
        encryption::{EncryptDecrypt, Encryptor},
        users::{UserManagement, UserManager},
    },
};

/// handles the encrpytion and storage of key material
pub fn launch(nsec: &Option<String>, password: &Option<String>) -> Result<nostr::Keys> {
    // if nsec parameter
    if let Some(nsec_unwrapped) = nsec {
        // get key or fail without prompts
        let key = nostr::Keys::from_sk_str(nsec_unwrapped).context("invalid nsec parameter")?;
        println!(
            "logged in as {}",
            &key.public_key()
                .to_bech32()
                .context("public key should always produce bech32")?
        );

        // if password, add user to enable password login in future
        if password.is_some() {
            UserManager::default()
                .add(nsec, password)
                .context("could not store identity")?;
        }
        return Ok(key);
    }

    // if encrypted nsec stored, attempt password
    let cfg = ConfigManager
        .load()
        .context("failed to load application config")?;
    let key = if let Some(user) = cfg.users.last() {
        let mut pass = if let Some(p) = password.clone() {
            p
        } else {
            println!(
                "login as {}",
                &user
                    .public_key
                    .to_bech32()
                    .context("public key should always produce bech32")?
            );
            Interactor::default()
                .password(PromptPasswordParms::default().with_prompt("password"))
                .context("failed to get password input from interactor.password")?
        };

        let key_result = Encryptor
            .decrypt_key(&user.encrypted_key, pass.as_str())
            .context("failed to decrypt key with provided password");
        pass.zeroize();

        key_result.context(format!(
            "failed to log in as {}",
            &user
                .public_key
                .to_bech32()
                .context("public key should always produce bech32")?
        ))?
    } else {
        // no nsec but password supplied
        if password.is_some() {
            bail!("no nsec available to decrypt with specified password");
        }
        // otherwise add new user with nsec and password prompts
        UserManager::default()
            .add(nsec, password)
            .context("failed to add user")?
    };
    println!(
        "logged in as {}",
        &key.public_key()
            .to_bech32()
            .context("public key should always produce bech32")?
    );

    // fetching metdata

    Ok(key)
}
