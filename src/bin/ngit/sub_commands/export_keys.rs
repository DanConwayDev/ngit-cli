use anyhow::{Context, Result};
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    login::{
        self, SignerInfo,
        existing::{get_signer_info, load_existing_login},
        fresh::generate_qr,
        logged_in_message,
    },
};
use nostr::prelude::ToBech32;

use crate::{cli::SignerParams, git::Repo};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// Print only the nsec or nbunksec, without opening the export menu
    #[arg(long, conflicts_with = "json", help_heading = "Export options")]
    pub secret: bool,
}

fn set_json_output(signer_info: &SignerInfo, npub: &str) -> Result<()> {
    match signer_info {
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            ..
        } => {
            let nbunksec = login::nbunksec::encode(bunker_uri, bunker_app_key)?;
            crate::output::set_value(serde_json::json!({
                "npub": npub,
                "nbunksec": nbunksec,
            }));
            Ok(())
        }
        SignerInfo::Nsec { nsec, .. } => {
            crate::output::set_value(serde_json::json!({
                "npub": npub,
                "nsec": nsec,
            }));
            Ok(())
        }
        SignerInfo::Selection { .. } => {
            anyhow::bail!("internal error: unresolved signer selection during key export")
        }
    }
}

pub async fn launch(args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    if args.secret && crate::output::is_json() {
        anyhow::bail!("--secret cannot be combined with --json");
    }
    let git_repo_result = Repo::discover().context("failed to find a git repository");
    let git_repo = { git_repo_result.ok() };

    let (signer_info, source, alias) =
        get_signer_info(&git_repo.as_ref(), signer.info, signer.password, &None)
            .await
            .map_err(login::require_account)?;
    let (_, user_ref, source) = load_existing_login(
        &git_repo.as_ref(),
        &Some(signer_info.clone()),
        signer.password,
        &Some(source),
        None,
        true,
        false,
        false,
    )
    .await
    .map_err(login::require_account)?;
    let npub = user_ref.public_key.to_bech32()?;
    let logged_in_msg = logged_in_message(&user_ref.metadata.name, &source, alias.as_deref());
    if crate::output::is_json() {
        return set_json_output(&signer_info, &npub);
    }
    if args.secret {
        return print_secret(&signer_info);
    }
    match signer_info {
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            ..
        } => {
            let nbunksec = login::nbunksec::encode(&bunker_uri, &bunker_app_key)?;
            export_interactive(&logged_in_msg, &npub, "nbunksec", &nbunksec)
        }
        SignerInfo::Nsec { nsec, .. } => export_interactive(&logged_in_msg, &npub, "nsec", &nsec),
        SignerInfo::Selection { .. } => {
            anyhow::bail!("internal error: unresolved signer selection during key export")
        }
    }
}

fn print_secret(signer_info: &SignerInfo) -> Result<()> {
    match signer_info {
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            ..
        } => println!("{}", login::nbunksec::encode(bunker_uri, bunker_app_key)?),
        SignerInfo::Nsec { nsec, .. } => println!("{nsec}"),
        SignerInfo::Selection { .. } => {
            anyhow::bail!("internal error: unresolved signer selection during key export")
        }
    }
    Ok(())
}

fn export_interactive(message: &str, npub: &str, secret_name: &str, secret: &str) -> Result<()> {
    match Interactor::default().choice(
        PromptChoiceParms::default()
            .with_default(0)
            .with_prompt(message)
            .with_choices(vec![
                "print npub".to_string(),
                "show QR code of npub".to_string(),
                format!("print {secret_name}"),
                format!("show QR code of {secret_name}"),
                "cancel".to_string(),
            ]),
    )? {
        0 => println!("{npub}"),
        1 => {
            for line in generate_qr(npub)? {
                println!("{line}");
            }
        }
        2 => println!("{secret}"),
        3 => {
            for line in generate_qr(secret)? {
                println!("{line}");
            }
        }
        _ => {}
    }
    Ok(())
}
