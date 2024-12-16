use anyhow::{Context, Result};
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    login::{
        SignerInfo, SignerInfoSource,
        existing::{get_signer_info, load_existing_login},
        fresh::generate_qr,
    },
};

use crate::git::Repo;

pub async fn launch() -> Result<()> {
    let git_repo_result = Repo::discover().context("failed to find a git repository");
    let git_repo = {
        match git_repo_result {
            Ok(git_repo) => Some(git_repo),
            Err(_) => None,
        }
    };

    if let Ok((signer_info, source)) = get_signer_info(&git_repo.as_ref(), &None, &None, &None) {
        if let Ok((_, user_ref, source)) = load_existing_login(
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
        {
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
                    return Ok(());
                }
                SignerInfo::Nsec {
                    nsec,
                    password: _,
                    npub: _,
                } => {
                    match Interactor::default().choice(
                        PromptChoiceParms::default()
                            .with_default(0)
                            .with_prompt(logged_in_msg)
                            .with_choices(vec![
                                "print nsec".to_string(),
                                "show QR code of nsec".to_string(),
                                "cancel".to_string(),
                            ]),
                    )? {
                        0 => {
                            println!("{nsec}");
                            return Ok(());
                        }
                        1 => {
                            for line in generate_qr(&nsec)? {
                                println!("{line}");
                            }
                            return Ok(());
                        }
                        _ => {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    eprintln!("not logged in so no keys are stored");
    Ok(())
}
