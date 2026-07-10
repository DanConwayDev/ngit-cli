use std::str::FromStr;

use anyhow::{Context, Result};
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    login::{
        self, SignerInfo, SignerInfoSource,
        existing::{get_signer_info, load_existing_login},
        fresh::generate_qr,
    },
};
use nostr::ToBech32;

use crate::git::Repo;

pub async fn launch() -> Result<()> {
    let git_repo_result = Repo::discover().context("failed to find a git repository");
    let git_repo = { git_repo_result.ok() };

    let (signer_info, source) =
        get_signer_info(&git_repo.as_ref(), &None, &None, &None).map_err(login::require_account)?;
    let (_, user_ref, source) = load_existing_login(
        &git_repo.as_ref(),
        &None,
        &None,
        &Some(source),
        None,
        true,
        false,
        false,
    )
    .await
    .map_err(login::require_account)?;
    let logged_in_msg = format!(
        "logged in {}as {}",
        if source == SignerInfoSource::GitLocal {
            "to local git repository "
        } else {
            ""
        },
        user_ref.metadata.name
    );
    match signer_info {
        SignerInfo::Bunker {
            bunker_uri: _,
            bunker_app_key: _,
            npub: _,
        } => {
            eprintln!(
                "failed: {logged_in_msg} using nostr connect so your keys are stored in a remote signer"
            );
            Ok(())
        }
        SignerInfo::Nsec {
            nsec,
            password: _,
            npub,
        } => {
            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_prompt(logged_in_msg)
                    .with_choices(vec![
                        "print npub".to_string(),
                        "show QR code of npub".to_string(),
                        "print nsec".to_string(),
                        "show QR code of nsec".to_string(),
                        "cancel".to_string(),
                    ]),
            )? {
                0 => {
                    let npub = if let Some(npub) = npub {
                        npub
                    } else {
                        nostr::Keys::from_str(&nsec)?.public_key().to_bech32()?
                    };
                    println!("{npub}");
                    Ok(())
                }
                1 => {
                    let npub = if let Some(npub) = npub {
                        npub
                    } else {
                        nostr::Keys::from_str(&nsec)?.public_key().to_bech32()?
                    };
                    for line in generate_qr(&npub)? {
                        println!("{line}");
                    }
                    Ok(())
                }
                2 => {
                    println!("{nsec}");
                    Ok(())
                }
                3 => {
                    for line in generate_qr(&nsec)? {
                        println!("{line}");
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
    }
}
