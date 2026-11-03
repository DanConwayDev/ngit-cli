use anyhow::{Context, Result, bail};
use ngit::{
    git::{get_git_config_item, remove_git_config_item},
    login::{
        SignerInfo, credential_store,
        existing::{resolve_selection, resolve_selector_npub},
        user,
    },
};

use crate::{cli::SignerParams, git::Repo};

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct SubCommandArgs {
    /// account to remove, even after its login selection has been cleared
    #[arg(value_name = "ACCOUNT", conflicts_with_all = ["profile", "signer"])]
    pub account: Option<String>,
    /// exact named signer profile to remove
    #[arg(long, value_name = "NAME", conflicts_with = "signer")]
    pub profile: Option<String>,
    /// remove only this repository's login; never fall back to global logout
    #[arg(long, conflicts_with = "global")]
    pub local: bool,
    /// remove only the global login, preserving this repository's login
    #[arg(long)]
    pub global: bool,
    /// also delete the selected stored credential, including an identity key
    #[arg(long, conflicts_with = "keep_credentials")]
    pub forget: bool,
    /// retain the remote session for later reactivation
    #[arg(long)]
    pub keep_credentials: bool,
}

const LOGIN_CONFIG_ITEMS: [&str; 5] = [
    "nostr.nsec",
    "nostr.npub",
    "nostr.bunker-uri",
    "nostr.bunker-app-key",
    "nostr.signer",
];

pub async fn launch(args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    let repo = Repo::discover().ok();
    if args.local && repo.is_none() {
        bail!("cannot log out locally outside a git repository");
    }
    let command_selector = match signer.info {
        Some(SignerInfo::Selection { selector }) => Some(selector.as_str()),
        Some(_) => {
            bail!("logout accepts a stored account via --signer, not new signing credentials")
        }
        None => None,
    };
    let selector = args
        .profile
        .as_deref()
        .or(args.account.as_deref())
        .or(command_selector);
    let target = if let Some(selector) = selector {
        Some(target_entries(repo.as_ref(), selector, args.profile.is_none()).await?)
    } else {
        None
    };
    let scopes = if args.global {
        vec![None]
    } else if args.local || std::env::var("NGITTEST").is_ok() {
        vec![repo.as_ref()]
    } else if repo.is_some() {
        vec![repo.as_ref(), None]
    } else {
        vec![None]
    };
    // Resolve every affected scope before deleting metadata that it may use.
    let mut selected_scopes = Vec::new();
    let mut entries = target.clone().unwrap_or_default();
    for scope in scopes {
        if !has_login_config(scope)? {
            continue;
        }
        let scoped_entries = scope_entries(scope).await;
        if let Some(target) = &target {
            let selected = get_git_config_item(&scope, "nostr.signer")?;
            let matches = selected.as_deref() == selector
                || scoped_entries
                    .as_ref()
                    .is_ok_and(|items| items.iter().any(|item| target.contains(item)));
            if !matches {
                continue;
            }
        } else {
            match scoped_entries {
                Ok(found) => entries = found,
                Err(error) if !args.forget => {
                    eprintln!("could not identify stored credentials; clearing only the selection: {error:#}");
                }
                Err(error) => return Err(error.context("cannot identify the credential to forget; select --profile NAME or use account forget-keys ENTRY")),
            }
        }
        selected_scopes.push(scope);
        if target.is_none() {
            break;
        }
    }
    if target.is_none() && selected_scopes.is_empty() {
        if args.forget {
            bail!(
                "no active login to forget; select the retained account with `ngit account logout ACCOUNT --forget` or --profile NAME"
            );
        }
        eprintln!("no login selected in the requested scope");
        report_remaining(repo.as_ref())?;
        return Ok(());
    }
    // Local selections can share credentials with other repositories. Only an
    // explicit --forget may delete their machine-wide stored credentials.
    let mut local_only =
        args.local || (target.is_none() && selected_scopes.first().is_some_and(Option::is_some));
    if !args.forget && args.global {
        if let Some(repo) = repo.as_ref() {
            local_only |= scope_entries(Some(repo))
                .await
                .is_ok_and(|local| local.iter().any(|entry| entries.contains(entry)));
        }
    }
    apply_logout(args, &selected_scopes, &entries, local_only)?;
    report_remaining(repo.as_ref())
}

fn apply_logout(
    args: &SubCommandArgs,
    selected_scopes: &[Option<&Repo>],
    entries: &[String],
    local_only: bool,
) -> Result<()> {
    let mut legacy_remote_entries = Vec::new();
    for scope in selected_scopes {
        if let Some(pointer) = get_git_config_item(scope, "nostr.bunker-app-key")? {
            if credential_store::parse_pointer(&pointer).is_some() {
                legacy_remote_entries.push(pointer);
            }
        }
    }
    let mut removed = 0;
    for entry in entries {
        if args.forget
            || (!args.keep_credentials
                && !local_only
                && (credential_store::is_bunker_signer_entry_name(entry)
                    || legacy_remote_entries.contains(entry)))
        {
            if credential_store::forget(entry)? {
                removed += 1;
            }
        } else {
            eprintln!(
                "stored credential retained; remove it with: ngit account forget-keys {entry}"
            );
        }
    }
    for scope in selected_scopes {
        for item in LOGIN_CONFIG_ITEMS {
            remove_git_config_item(scope, item)
                .with_context(|| format!("failed to clear login item {item}"))?;
        }
        eprintln!(
            "cleared {} login selection",
            if scope.is_some() {
                "repository-local"
            } else {
                "global"
            }
        );
    }
    if removed > 0 {
        eprintln!(
            "removed {removed} stored credential(s); remote signer permissions are unchanged"
        );
    } else if args.forget {
        eprintln!("no stored credential remains for the selected account");
    }
    if let Err(error) = user::wipe_private_git_relay_list_cache() {
        eprintln!("warning: failed to remove cached decrypted private relay lists: {error:#}");
    }
    Ok(())
}

async fn target_entries(
    repo: Option<&Repo>,
    selector: &str,
    allow_name: bool,
) -> Result<Vec<String>> {
    let target = match resolve_selector_npub(&repo, selector) {
        Ok(target) => target,
        Err(error) if !allow_name => return Err(error),
        Err(_) => {
            // Display-name lookup uses cached metadata and stored credentials;
            // it never asks the remote signer to authenticate removal.
            let selected = resolve_selection(&repo, selector, &None, true).await?;
            resolve_selector_npub(&repo, selected.alias.as_deref().unwrap_or(&selected.npub))?
        }
    };
    credential_store::selection_entries(&target.npub, target.credential.as_deref())
}

async fn scope_entries(scope: Option<&Repo>) -> Result<Vec<String>> {
    // The explicit selector is authoritative over leftover legacy fields.
    if let Some(selector) = get_git_config_item(&scope, "nostr.signer")? {
        return target_entries(scope, &selector, false).await;
    }
    // Legacy direct pointers remain removable even when their record is bad.
    let mut pointers = Vec::new();
    for item in ["nostr.nsec", "nostr.bunker-app-key"] {
        if let Some(value) = get_git_config_item(&scope, item)? {
            if credential_store::parse_pointer(&value).is_some() {
                pointers.push(value);
            }
        }
    }
    if !pointers.is_empty() {
        return Ok(pointers);
    }
    Ok(Vec::new())
}

fn has_login_config(scope: Option<&Repo>) -> Result<bool> {
    for item in LOGIN_CONFIG_ITEMS {
        if get_git_config_item(&scope, item)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn report_remaining(repo: Option<&Repo>) -> Result<()> {
    if let Some(repo) = repo {
        if has_login_config(Some(repo))? {
            eprintln!(
                "this repository still has a local login selection; inspect it with `ngit account whoami`"
            );
            return Ok(());
        }
    }
    if has_login_config(None)? {
        eprintln!(
            "{}global login remains selected; inspect it with `ngit account whoami`",
            if repo.is_some() {
                "this repository now inherits the "
            } else {
                "the "
            }
        );
    } else {
        eprintln!(
            "no local or global login remains selected; retained credentials may still be listed by `ngit account whoami`"
        );
    }
    Ok(())
}
