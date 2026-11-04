use anyhow::{Context, Result, bail};
use ngit::{
    git::{get_git_config_item, get_git_config_item_system, remove_git_config_item},
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
    /// account to remove, even after logout; an explicit npub removes all
    /// saved credentials and profiles for that identity
    #[arg(value_name = "ACCOUNT", conflicts_with_all = ["profile", "signer"])]
    pub account: Option<String>,
    /// exact named signer profile to remove
    #[arg(long, value_name = "NAME", conflicts_with = "signer")]
    pub profile: Option<String>,
    /// remove all known saved profiles and current/non-local Git logins;
    /// do not search other repositories or revoke remote permissions
    #[arg(long, conflicts_with_all = ["local", "global", "profile", "account", "signer"])]
    pub all: bool,
    /// remove the repository-specific login and credentials; a remaining
    /// default account becomes active; never remove the default login
    #[arg(long, conflicts_with = "global")]
    pub local: bool,
    /// remove the default login and credentials; preserve repository-specific
    /// selections (shared credentials become unavailable)
    #[arg(long)]
    pub global: bool,
    /// permit deleting identity private keys; otherwise their deletion fails
    /// without prompting or changing anything
    #[arg(long, conflicts_with = "keep_credentials")]
    pub forget: bool,
    /// clear the login selection but retain credentials for later reactivation
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
    if args.all {
        if signer.info.is_some() {
            bail!("logout --all does not accept signing credentials");
        }
        return super::logout_all::launch(args, repo.as_ref()).await;
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
        Some(
            if args.profile.is_none() && credential_store::parse_pointer(selector) == Some(selector)
            {
                credential_store::account_entries(selector)?
            } else {
                target_entries(repo.as_ref(), selector, args.profile.is_none()).await?
            },
        )
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
                Err(error) if args.keep_credentials => {
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
        return list_saved_profiles().await;
    }
    apply_logout(args, &selected_scopes, &entries)?;
    report_remaining(repo.as_ref())?;
    list_saved_profiles().await
}

pub(super) async fn list_saved_profiles() -> Result<()> {
    if !crate::output::is_json() {
        eprintln!("remaining accounts and saved profiles:");
    }
    super::whoami::launch(
        &super::whoami::SubCommandArgs { offline: true },
        crate::output::is_json(),
    )
    .await
}

fn apply_logout(
    args: &SubCommandArgs,
    selected_scopes: &[Option<&Repo>],
    entries: &[String],
) -> Result<()> {
    let mut legacy_remote_entries = Vec::new();
    let mut identity_entries = Vec::new();
    for scope in selected_scopes {
        if args.keep_credentials
            && (get_git_config_item(scope, "nostr.bunker-uri")?.is_some()
                || get_git_config_item(scope, "nostr.bunker-app-key")?.is_some())
        {
            bail!(
                "cannot retain a remote signer connection stored in Git config automatically; nothing changed. Export the connection first, then omit --keep-credentials to remove its local configuration"
            );
        }
        if let Some(value) = get_git_config_item(scope, "nostr.nsec")? {
            if credential_store::parse_pointer(&value).is_some() {
                identity_entries.push(value);
            }
        }
        if let Some(pointer) = get_git_config_item(scope, "nostr.bunker-app-key")? {
            if credential_store::parse_pointer(&pointer).is_some() {
                legacy_remote_entries.push(pointer);
            }
        }
    }
    if !args.forget
        && !args.keep_credentials
        && entries.iter().any(|entry| {
            credential_store::parse_pointer(entry).is_some()
                && (identity_entries.contains(entry) || !legacy_remote_entries.contains(entry))
        })
    {
        bail!(
            "logout would delete a saved identity key; nothing changed. Use --forget to delete it or --keep-credentials to retain it"
        );
    }
    preserve_plaintext_keys(args, selected_scopes)?;
    let mut removed = 0;
    if args.keep_credentials {
        eprintln!("saved credentials retained at your request");
    } else {
        // Credentials must be deleted before their alias metadata, so removal
        // guards and account inventory can still be resolved from the bindings.
        removed = credential_store::forget_entries(entries)?;
    }
    for scope in selected_scopes {
        for item in LOGIN_CONFIG_ITEMS {
            remove_git_config_item(scope, item)
                .with_context(|| format!("failed to clear login item {item}"))?;
        }
        eprintln!(
            "removed {} login",
            if scope.is_some() {
                "this repository-specific"
            } else {
                "the default"
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

fn preserve_plaintext_keys(args: &SubCommandArgs, scopes: &[Option<&Repo>]) -> Result<()> {
    if args.forget {
        return Ok(());
    }
    let mut keys = Vec::new();
    for scope in scopes {
        if let Some(value) = get_git_config_item(scope, "nostr.nsec")? {
            if credential_store::parse_pointer(&value).is_none() && !value.is_empty() {
                if !args.keep_credentials {
                    bail!(
                        "logout would delete an identity key from Git config; nothing changed. Use --forget to delete it or --keep-credentials to preserve it in the credential store"
                    );
                }
                keys.push(nostr::prelude::Keys::parse(&value).context("cannot retain an encrypted or invalid Git-config key automatically; export it first, then use logout --forget")?);
            }
        }
    }
    for key in keys {
        credential_store::store(&key, credential_store::SecretStorage::Auto)?;
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
                "this repository still has a repository-specific account selected; its availability is listed below"
            );
            return Ok(());
        }
    }
    if has_login_config(None)? {
        eprintln!(
            "{}; its availability is listed below",
            if repo.is_some() {
                "this repository now uses your default account selection"
            } else {
                "your default account remains selected"
            }
        );
    } else if LOGIN_CONFIG_ITEMS
        .iter()
        .map(|key| get_git_config_item_system(key))
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(Option::is_some)
    {
        eprintln!("a system-wide account remains selected; its availability is listed below");
    } else {
        eprintln!("you are now logged out; any other saved accounts are listed below");
    }
    Ok(())
}
