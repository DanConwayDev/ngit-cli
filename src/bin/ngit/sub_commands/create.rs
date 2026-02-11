use anyhow::{Context, Result};
use clap::Parser;
use ngit::client::Params;
use nostr_sdk::ToBech32;

use crate::{
    cli::Cli,
    client::{Client, Connect},
    git::Repo,
    login::fresh::signup_non_interactive,
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

    /// Save credentials only to local git config
    #[arg(long)]
    pub local: bool,
}

pub async fn launch(_cli: &Cli, args: &SubCommandArgs) -> Result<()> {
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

    let (_signer, public_key, _signer_info, keys) = signup_non_interactive(
        args.name.clone(),
        client.as_ref(),
        args.local,
        publish,
        relay_urls,
    )
    .await
    .context("failed to create account")?;

    // Display the generated nsec prominently
    println!("\n✓ Account created successfully!");
    println!("\nDisplay name: {}", args.name);
    println!("Public key (npub): {}", public_key.to_bech32()?);
    println!("\n⚠️  IMPORTANT: Save your secret key (nsec) securely!");
    println!("nsec: {}", keys.secret_key().to_bech32()?);
    println!("\nYou will need this key to log in from other devices.");
    println!("Run 'ngit account export-keys' to see this again.\n");

    if publish {
        println!("✓ Published metadata to relays");
    }

    if args.local {
        println!("✓ Saved credentials to local git config only");
    } else {
        println!("✓ Saved credentials to global git config");
    }

    // Disconnect client if it was created
    if let Some(client) = client {
        client.disconnect().await?;
    }

    Ok(())
}
