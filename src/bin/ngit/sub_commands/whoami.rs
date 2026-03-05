use anyhow::{Context, Result};
use ngit::{
    client::Params,
    login::{
        SignerInfoSource,
        existing::{get_signer_info, load_existing_login},
    },
};
use nostr_sdk::ToBech32;
use serde::Serialize;

use crate::{
    cli::{Cli, extract_signer_cli_arguments},
    client::{Client, Connect},
    git::Repo,
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// use local cache only, skip network fetch
    #[arg(long, action)]
    pub offline: bool,

    /// output as JSON
    #[arg(long, action)]
    pub json: bool,
}

#[derive(Serialize)]
struct UserJson {
    name: String,
    npub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    nip05: Option<String>,
    scope: String,
}

#[derive(Serialize)]
struct WhoamiJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    local: Option<UserJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    global: Option<UserJson>,
    /// The account that would be used for operations in the current context
    /// (local takes priority over global).
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<UserJson>,
}

pub async fn launch(args: &Cli, command_args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover()
        .context("failed to find a git repository")
        .ok();

    let client = if command_args.offline {
        None
    } else {
        Some(Client::new(Params::with_git_config_relay_defaults(
            &git_repo.as_ref(),
        )))
    };

    let signer_info = extract_signer_cli_arguments(args).unwrap_or(None);

    // Try to load local login (silent, no prompts)
    let local = load_user_for_scope(
        git_repo.as_ref(),
        signer_info.as_ref(),
        client.as_ref(),
        SignerInfoSource::GitLocal,
    )
    .await;

    // Try to load global login (silent, no prompts)
    let global = load_user_for_scope(
        git_repo.as_ref(),
        signer_info.as_ref(),
        client.as_ref(),
        SignerInfoSource::GitGlobal,
    )
    .await;

    if let Some(client) = client {
        client.disconnect().await?;
    }

    if command_args.json {
        // active = local if present, else global
        let active = local
            .as_ref()
            .map(|u| make_user_json(u, "local"))
            .or_else(|| global.as_ref().map(|u| make_user_json(u, "global")));

        let output = WhoamiJson {
            local: local.as_ref().map(|u| make_user_json(u, "local")),
            global: global.as_ref().map(|u| make_user_json(u, "global")),
            active,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        match (local.as_ref(), global.as_ref()) {
            (None, None) => {
                println!("not logged in");
                println!();
                println!("use `ngit account login` to log in");
            }
            (Some(u), None) => {
                println!("logged in to local repository as:");
                print_user_human(u);
            }
            (None, Some(u)) => {
                println!("logged in globally as:");
                print_user_human(u);
            }
            (Some(local_u), Some(global_u)) => {
                println!("local (active):");
                print_user_human(local_u);
                println!();
                println!("global:");
                print_user_human(global_u);
            }
        }
    }

    Ok(())
}

fn make_user_json(u: &(String, String, Option<String>), scope: &str) -> UserJson {
    UserJson {
        name: u.0.clone(),
        npub: u.1.clone(),
        nip05: u.2.clone(),
        scope: scope.to_string(),
    }
}

fn print_user_human(u: &(String, String, Option<String>)) {
    let (name, npub, nip05) = u;
    println!("  name: {name}");
    println!("  npub: {npub}");
    if let Some(nip05) = nip05 {
        println!("  nip05: {nip05}");
    }
}

/// Attempt to silently load a user from a specific config scope.
/// Returns `Some((name, npub, nip05))` on success, `None` if not logged in
/// via that scope or if the scope requires a password prompt (ncryptsec).
async fn load_user_for_scope(
    git_repo: Option<&Repo>,
    signer_info: Option<&ngit::login::SignerInfo>,
    client: Option<&Client>,
    source: SignerInfoSource,
) -> Option<(String, String, Option<String>)> {
    // First verify signer info exists for this scope without building a full
    // signer — avoids triggering password prompts for ncryptsec.
    if get_signer_info(
        &git_repo,
        &signer_info.cloned(),
        &None,
        &Some(source.clone()),
    )
    .is_err()
    {
        return None;
    }

    let result = load_existing_login(
        &git_repo,
        &signer_info.cloned(),
        &None,
        &Some(source),
        client,
        true,  // silent — don't print "logged in as"
        false, // don't prompt for password (ncryptsec users get None here)
        false, // don't force a relay fetch if already cached
    )
    .await;

    match result {
        Ok((_, user_ref, _)) => {
            let npub = user_ref.public_key.to_bech32().ok()?;
            Some((user_ref.metadata.name, npub, user_ref.metadata.nip05))
        }
        Err(_) => None,
    }
}
