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
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<UserJson>,
    /// The account that would be used for operations in the current context
    /// (local > global > system, matching git's priority order).
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

    // Try to load login from each config level (silent, no prompts)
    let local = load_user_for_scope(
        git_repo.as_ref(),
        signer_info.as_ref(),
        client.as_ref(),
        SignerInfoSource::GitLocal,
    )
    .await;

    let global = load_user_for_scope(
        git_repo.as_ref(),
        signer_info.as_ref(),
        client.as_ref(),
        SignerInfoSource::GitGlobal,
    )
    .await;

    let system = load_user_for_scope(
        git_repo.as_ref(),
        signer_info.as_ref(),
        client.as_ref(),
        SignerInfoSource::GitSystem,
    )
    .await;

    if let Some(client) = client {
        client.disconnect().await?;
    }

    // Active account follows git's priority order: local > global > system
    let active_scope = if local.is_some() {
        Some("local")
    } else if global.is_some() {
        Some("global")
    } else if system.is_some() {
        Some("system")
    } else {
        None
    };

    if command_args.json {
        let active = active_scope.and_then(|scope| match scope {
            "local" => local.as_ref().map(|u| make_user_json(u, scope)),
            "global" => global.as_ref().map(|u| make_user_json(u, scope)),
            "system" => system.as_ref().map(|u| make_user_json(u, scope)),
            _ => None,
        });

        let output = WhoamiJson {
            local: local.as_ref().map(|u| make_user_json(u, "local")),
            global: global.as_ref().map(|u| make_user_json(u, "global")),
            system: system.as_ref().map(|u| make_user_json(u, "system")),
            active,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if local.is_none() && global.is_none() && system.is_none() {
        println!("not logged in");
        println!();
        println!("use `ngit account login` to log in");
    } else {
        type UserEntry = Option<(String, String, Option<String>)>;
        let entries: &[(&str, &UserEntry)] =
            &[("local", &local), ("global", &global), ("system", &system)];
        let mut first = true;
        for (scope, user) in entries {
            if let Some(u) = user {
                if !first {
                    println!();
                }
                first = false;
                let is_active = active_scope == Some(scope);
                if is_active {
                    println!("{scope} (active):");
                } else {
                    println!("{scope}:");
                }
                print_user_human(u);
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
