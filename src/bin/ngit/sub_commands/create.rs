use anyhow::{Context, Result};
use clap::Parser;
use ngit::{client::Params, login::credential_store};
use nostr::prelude::ToBech32;

use crate::{
    cli::Cli,
    client::{Client, Connect},
    git::Repo,
    login::fresh::{configured_signer_scope_message, signup_non_interactive},
};

#[derive(Parser)]
pub struct SubCommandArgs {
    /// Display name for the new account
    #[arg(long, required = true)]
    pub name: String,

    /// Relay URLs for the new account's relay list (can be specified multiple
    /// times). Defaults to the relay-default-set if not provided.
    #[arg(long = "relay", value_parser, num_args = 1)]
    pub relays: Vec<String>,

    /// Don't publish metadata to relays (offline mode)
    #[arg(long)]
    pub offline: bool,

    /// Use the new account only in this local Git repository
    #[arg(long)]
    pub local: bool,

    /// Where to store the account secret: auto (OS credential store, falling
    /// back to ngit's file store), file, or git-config (plaintext)
    #[arg(long, value_name = "auto|file|git-config")]
    pub secret_storage: Option<String>,
}

pub async fn launch(_cli: &Cli, args: &SubCommandArgs) -> Result<()> {
    if let Some(value) = &args.secret_storage {
        let policy = credential_store::parse_policy(value).with_context(|| {
            format!("invalid --secret-storage value '{value}'; expected auto, file or git-config")
        })?;
        credential_store::set_policy_override(policy);
    }
    let git_repo = Repo::discover().ok();

    let params = Params::with_git_config_relay_defaults(&git_repo.as_ref());

    let relay_urls = if args.relays.is_empty() {
        params.relay_default_set.clone()
    } else {
        args.relays.clone()
    };

    let client = if args.offline {
        None
    } else {
        Some(Client::new(params))
    };

    let publish = !args.offline;

    let (_signer, public_key, signer_info, _keys) = signup_non_interactive(
        args.name.clone(),
        client.as_ref(),
        args.local,
        publish,
        relay_urls,
    )
    .await
    .context("failed to create account")?;

    let npub = public_key.to_bech32()?;
    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "created",
            "entity": "account",
            "name": args.name,
            "npub": npub,
            "scope": if args.local { "local" } else { "global" },
            "published": publish,
        }));
    } else {
        println!("\n✓ Account created successfully!");
        println!("\nDisplay name: {}", args.name);
        println!("Public key (npub): {npub}");
        println!("\nYour secret key (nsec) has been stored securely.");
        println!("Run 'ngit account export-keys' to view it.\n");

        if publish {
            println!("✓ Published metadata to relays");
        }

        println!(
            "✓ {}",
            configured_signer_scope_message(!args.local, &signer_info)
        );
    }

    // Disconnect client if it was created
    if let Some(client) = client {
        client.disconnect().await?;
    }

    Ok(())
}
