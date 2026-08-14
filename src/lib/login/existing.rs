use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use nostr::prelude::{Event, Filter, Kind, Metadata, PublicKey, ToBech32, nip46::NostrConnectUri};
use nostr_connect::client::NostrConnect;

use super::{
    SignerInfo, SignerInfoSource, credential_store,
    key_encryption::decrypt_key,
    print_logged_in_as,
    user::{UserRef, get_user_details},
};
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptPasswordParms},
    client::{fetch_public_key, get_event_from_global_cache},
    git::{Repo, RepoActions, get_git_config_item, get_git_config_item_system},
};

#[derive(Debug)]
pub(super) struct SignerInfoNotFound;

impl std::fmt::Display for SignerInfoNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "failed to get or find signer info in cli arguments, local git config, global git config or system git config",
        )
    }
}

impl std::error::Error for SignerInfoNotFound {}

#[derive(Debug)]
pub struct SignerAliasNotFound {
    alias: String,
}

impl std::fmt::Display for SignerAliasNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "signer alias '{}' is not defined in the OS credential store, credentials.json, or git config",
            self.alias
        )
    }
}

impl std::error::Error for SignerAliasNotFound {}

/// load signer from git config and UserProfile from cache or relays
///
/// # Parameters
/// - `client`: include client to fetch profiles from relays that are missing
///   from cache
/// - `silent`: do not print outcome in termianl
#[allow(clippy::too_many_arguments)]
pub async fn load_existing_login(
    git_repo: &Option<&Repo>,
    signer_info: &Option<SignerInfo>,
    password: &Option<String>,
    source: &Option<SignerInfoSource>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    silent: bool,
    prompt_for_password: bool,
    fetch_profile_updates: bool,
) -> Result<(Arc<crate::NgitSigner>, UserRef, SignerInfoSource)> {
    let (signer_info, source, alias) =
        get_signer_info(git_repo, signer_info, password, source).await?;

    let (signer, public_key) = get_signer(&signer_info, prompt_for_password).await?;

    let user_ref = get_user_details(
        &public_key,
        client,
        if let Some(git_repo) = git_repo {
            Some(git_repo.get_path()?)
        } else {
            None
        },
        silent,
        fetch_profile_updates,
    )
    .await?;

    if !silent {
        print_logged_in_as(&user_ref, client.is_none(), &source, alias.as_deref())?;
    }
    Ok((signer, user_ref, source))
}

/// The alias a persisted `nostr.signer` selector names, for display.
///
/// Config-scoped `nostr.signer` values keep strict npub/alias semantics, so
/// the selector's shape alone identifies an alias; callers display it after
/// the login has already resolved it. One-shot command-line selections are
/// never persisted, so their alias comes from [`get_signer_info`] at
/// resolution time instead.
pub fn selected_alias(
    git_repo: &Option<&Repo>,
    source: &SignerInfoSource,
) -> Result<Option<String>> {
    let selector = match source {
        SignerInfoSource::CommandLineArguments => None,
        SignerInfoSource::GitLocal => {
            let repo = git_repo.context("cannot read local signer alias without a repository")?;
            get_git_config_item(&Some(repo), "nostr.signer")?
        }
        SignerInfoSource::GitGlobal => get_git_config_item(&None, "nostr.signer")?,
        SignerInfoSource::GitSystem => get_git_config_item_system("nostr.signer")?,
    };
    Ok(selector.filter(|selector| {
        !selector.starts_with("npub1") && credential_store::normalize_alias(selector).is_ok()
    }))
}

/// priority order: cli arguments, local git config, global git config, system
/// git config
///
/// The third element is the normalized alias the selector resolved through,
/// captured here so display code never re-probes the credential store.
pub async fn get_signer_info(
    git_repo: &Option<&Repo>,
    signer_info: &Option<SignerInfo>,
    password: &Option<String>,
    source: &Option<SignerInfoSource>,
) -> Result<(SignerInfo, SignerInfoSource, Option<String>)> {
    Ok(match source {
        None => {
            let mut result = None;
            for source in if std::env::var("NGITTEST").is_ok() {
                vec![
                    SignerInfoSource::CommandLineArguments,
                    SignerInfoSource::GitLocal,
                ]
            } else {
                vec![
                    SignerInfoSource::CommandLineArguments,
                    SignerInfoSource::GitLocal,
                    SignerInfoSource::GitGlobal,
                    SignerInfoSource::GitSystem,
                ]
            } {
                // recursion inside an async fn needs boxing to keep the
                // future's size computable
                match Box::pin(get_signer_info(
                    git_repo,
                    signer_info,
                    password,
                    &Some(source.clone()),
                ))
                .await
                {
                    Ok(res) => {
                        result = Some(res);
                        break;
                    }
                    Err(error) => {
                        let explicit_cli_selection = source
                            == SignerInfoSource::CommandLineArguments
                            && matches!(signer_info, Some(SignerInfo::Selection { .. }));
                        let configured_selection = match source {
                            SignerInfoSource::GitLocal => git_repo.is_some_and(|repo| {
                                get_git_config_item(&Some(repo), "nostr.signer")
                                    .is_ok_and(|value| value.is_some())
                            }),
                            SignerInfoSource::GitGlobal => {
                                get_git_config_item(&None, "nostr.signer")
                                    .is_ok_and(|value| value.is_some())
                            }
                            SignerInfoSource::GitSystem => {
                                get_git_config_item_system("nostr.signer")
                                    .is_ok_and(|value| value.is_some())
                            }
                            SignerInfoSource::CommandLineArguments => false,
                        };
                        if explicit_cli_selection
                            || configured_selection
                            || error
                                .downcast_ref::<credential_store::LookupError>()
                                .is_some()
                        {
                            return Err(error);
                        }
                    }
                }
            }
            result.ok_or(SignerInfoNotFound)?
        }
        Some(SignerInfoSource::CommandLineArguments) => {
            if let Some(signer_info) = signer_info {
                let (signer_info, alias) = match signer_info {
                    SignerInfo::Selection { selector } => {
                        let resolved =
                            resolve_selection(git_repo, selector, password, true).await?;
                        (resolved.signer_info, resolved.alias)
                    }
                    signer_info => (signer_info.clone(), None),
                };
                (signer_info, SignerInfoSource::CommandLineArguments, alias)
            } else {
                bail!("failed to get signer from cli signer arguments because none were specified")
            }
        }
        Some(SignerInfoSource::GitLocal) => {
            let git_repo =
                git_repo.context("failed to get local git config as no git_repo supplied")?;
            if let Some(selector) = get_git_config_item(&Some(git_repo), "nostr.signer")
                .context("failed get local git config")?
            {
                let resolved =
                    resolve_selection(&Some(git_repo), &selector, password, false).await?;
                (
                    resolved.signer_info,
                    SignerInfoSource::GitLocal,
                    resolved.alias,
                )
            } else if let Ok(nsec) = get_git_config_item(&Some(git_repo), "nostr.nsec")
                .context("failed get local git config")?
                .context("git local config item nostr.nsec doesn't exist")
            {
                let nsec = resolve_config_secret(&Some(git_repo), &nsec, true, true)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item(&Some(git_repo), "nostr.npub")
                            .context("failed get local git config")?,
                        verify_npub: false,
                    },
                    SignerInfoSource::GitLocal,
                    None,
                )
            } else if let Ok(bunker_uri) = get_git_config_item(&Some(git_repo), "nostr.bunker-uri")
                .context("failed get local git config")?
                .context("git local config item nostr.bunker-uri doesn't exist")
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&Some(git_repo), &get_git_config_item(&Some(git_repo), "nostr.bunker-app-key")
                    .context("failed get local git config")?
                    .context("git local config item nostr.bunker-uri exists but nostr.bunker-app-key doesn't")?, false, true)?,
                    npub: get_git_config_item(&Some(git_repo), "nostr.npub")
                        .context("failed get local git config")?,
                }, SignerInfoSource::GitLocal, None)
            } else {
                bail!("no signer info in local git config")
            }
        }
        Some(SignerInfoSource::GitGlobal) => {
            if let Some(selector) = get_git_config_item(&None, "nostr.signer")
                .context("failed to get global git config")?
            {
                let resolved = resolve_selection(git_repo, &selector, password, false).await?;
                (
                    resolved.signer_info,
                    SignerInfoSource::GitGlobal,
                    resolved.alias,
                )
            } else if let Some(nsec) = get_git_config_item(&None, "nostr.nsec")
                .context("failed to get global git config")?
            {
                let nsec = resolve_config_secret(&None, &nsec, true, true)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item(&None, "nostr.npub")
                            .context("failed to get global git config")?,
                        verify_npub: false,
                    },
                    SignerInfoSource::GitGlobal,
                    None,
                )
            } else if let Some(bunker_uri) = get_git_config_item(&None, "nostr.bunker-uri")
                .context("failed to get global git config")?
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&None, &get_git_config_item(&None, "nostr.bunker-app-key")
                    .context("failed get local git config")?
                    .context("git global config item nostr.bunker-uri exists but nostr.bunker-app-key doesn't")?, false, true)?,
                    npub: get_git_config_item(&None, "nostr.npub")
                        .context("failed get global git config")?,
                }, SignerInfoSource::GitGlobal, None)
            } else {
                bail!("no signer info in global git config")
            }
        }
        Some(SignerInfoSource::GitSystem) => {
            if let Some(selector) = get_git_config_item_system("nostr.signer")
                .context("failed to get system git config")?
            {
                let resolved = resolve_selection(git_repo, &selector, password, false).await?;
                (
                    resolved.signer_info,
                    SignerInfoSource::GitSystem,
                    resolved.alias,
                )
            } else if let Some(nsec) = get_git_config_item_system("nostr.nsec")
                .context("failed to get system git config")?
            {
                let nsec = resolve_config_secret(&None, &nsec, true, false)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item_system("nostr.npub")
                            .context("failed to get system git config")?,
                        verify_npub: false,
                    },
                    SignerInfoSource::GitSystem,
                    None,
                )
            } else if let Some(bunker_uri) = get_git_config_item_system("nostr.bunker-uri")
                .context("failed to get system git config")?
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&None, &get_git_config_item_system("nostr.bunker-app-key")
                    .context("failed to get system git config")?
                    .context("system git config item nostr.bunker-uri exists but nostr.bunker-app-key doesn't")?, false, false)?,
                    npub: get_git_config_item_system("nostr.npub")
                        .context("failed to get system git config")?,
                }, SignerInfoSource::GitSystem, None)
            } else {
                bail!("no signer info in system git config")
            }
        }
    })
}

#[derive(Clone, Copy)]
enum ConfigScope {
    Local,
    Global,
    System,
}

impl ConfigScope {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
            Self::System => "system",
        }
    }
}

fn selection_scopes(git_repo: &Option<&Repo>) -> Vec<ConfigScope> {
    let mut scopes = Vec::with_capacity(3);
    if git_repo.is_some() {
        scopes.push(ConfigScope::Local);
    }
    if std::env::var("NGITTEST").is_err() {
        scopes.extend([ConfigScope::Global, ConfigScope::System]);
    }
    scopes
}

fn config_value(git_repo: &Option<&Repo>, scope: ConfigScope, key: &str) -> Result<Option<String>> {
    match scope {
        ConfigScope::Local => {
            let repo = git_repo.context("cannot read local git config without a repository")?;
            get_git_config_item(&Some(repo), key).context("failed to read local git config")
        }
        ConfigScope::Global => {
            get_git_config_item(&None, key).context("failed to read global git config")
        }
        ConfigScope::System => {
            get_git_config_item_system(key).context("failed to read system git config")
        }
    }
}

fn resolve_scope_secret(
    git_repo: &Option<&Repo>,
    scope: ConfigScope,
    value: &str,
    is_nsec: bool,
) -> Result<String> {
    resolve_config_secret(
        if matches!(scope, ConfigScope::Local) {
            git_repo
        } else {
            &None
        },
        value,
        is_nsec,
        !matches!(scope, ConfigScope::System),
    )
}

/// A signer selector resolved to concrete signer material.
#[derive(Debug)]
pub struct ResolvedSelection {
    pub signer_info: SignerInfo,
    /// canonical npub of the selected identity — the only form of a
    /// selection that may be persisted
    pub npub: String,
    /// the normalized alias the selector resolved through, when it was one
    pub alias: Option<String>,
}

/// Resolve a selector with npub → alias → cached-profile-name precedence.
///
/// Profile names are mutable and non-unique, so they are a selection-time
/// convenience only: callers that persist the selection must write
/// [`ResolvedSelection::npub`] (or the matched alias), never the selector
/// text.
pub async fn resolve_selection(
    git_repo: &Option<&Repo>,
    selector: &str,
    password: &Option<String>,
    allow_profile_name: bool,
) -> Result<ResolvedSelection> {
    match resolve_selector_npub(git_repo, selector) {
        Ok(npub) => {
            let alias = if selector.starts_with("npub1") {
                None
            } else {
                credential_store::normalize_alias(selector).ok()
            };
            let signer_info =
                resolve_signer_for_npub(git_repo, &npub, password)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "selected signer {npub} is not available in the OS credential store, credentials.json, or git config"
                    )
                })?;
            Ok(ResolvedSelection {
                signer_info,
                npub,
                alias,
            })
        }
        Err(error) if allow_profile_name && selector_may_be_profile_name(selector, &error) => {
            resolve_selection_by_profile_name(git_repo, selector, password).await
        }
        Err(error) => Err(error),
    }
}

/// Only two alias-resolution outcomes may fall through to profile-name
/// lookup: a valid alias token with no mapping anywhere, and a selector that
/// can never be an alias token. Store `Unavailable` / `Invalid` errors keep
/// alias resolution authoritative and propagate unchanged, and an invalid
/// npub is always an npub error.
fn selector_may_be_profile_name(selector: &str, error: &anyhow::Error) -> bool {
    !selector.starts_with("npub1")
        && (error.downcast_ref::<SignerAliasNotFound>().is_some()
            || credential_store::normalize_alias(selector).is_err())
}

async fn resolve_selection_by_profile_name(
    git_repo: &Option<&Repo>,
    selector: &str,
    password: &Option<String>,
) -> Result<ResolvedSelection> {
    let git_repo_path = if let Some(git_repo) = git_repo {
        Some(git_repo.get_path()?)
    } else {
        None
    };
    let events =
        get_event_from_global_cache(git_repo_path, vec![Filter::default().kind(Kind::Metadata)])
            .await
            .context(
                "failed to read cached profiles while resolving the selected signer by name",
            )?;
    select_credentialed_profile(selector, &events, |npub| {
        resolve_signer_for_npub(git_repo, npub, password)
    })
}

/// Pick the single cached profile named `selector` whose npub `probe`
/// resolves to usable signer material.
///
/// A candidate without stored credentials (`Ok(None)`) is filtered out —
/// this is what makes cached name-squatting harmless — but a broken or
/// unavailable entry (`Err`) fails the whole selection instead of being
/// skipped, because skipping it could silently select a different
/// same-named account. More than one credentialed match fails closed.
fn select_credentialed_profile(
    selector: &str,
    events: &[Event],
    mut probe: impl FnMut(&str) -> Result<Option<SignerInfo>>,
) -> Result<ResolvedSelection> {
    let candidates = cached_profile_candidates(selector, events);
    if candidates.is_empty() {
        bail!(
            "no cached profile is named '{selector}'; profiles enter ngit's cache when their account logs in. Select the signer with `--signer <npub>` or a signer alias instead"
        );
    }
    let mut credentialed = Vec::new();
    for candidate in candidates {
        match probe(&candidate.npub) {
            Ok(Some(signer_info)) => credentialed.push((candidate, signer_info)),
            Ok(None) => {}
            Err(error) => {
                return Err(error.context(format!(
                    "failed to check stored signer credentials for cached profile '{}' ({}); fix or remove that entry, or select the signer with `--signer <npub>`",
                    candidate.label, candidate.npub
                )));
            }
        }
    }
    match credentialed.len() {
        0 => bail!(
            "no stored signer credentials belong to a cached profile named '{selector}'; log that account in first, or select the signer with `--signer <npub>` or a signer alias"
        ),
        1 => {
            let (candidate, signer_info) = credentialed.remove(0);
            Ok(ResolvedSelection {
                signer_info,
                npub: candidate.npub,
                alias: None,
            })
        }
        _ => {
            let listing = credentialed
                .iter()
                .map(|(candidate, _)| format!("\n  {} ({})", candidate.label, candidate.npub))
                .collect::<String>();
            bail!(
                "profile name '{selector}' matches more than one stored signer:{listing}\nselect one with `--signer <npub>` or a signer alias instead"
            )
        }
    }
}

struct ProfileCandidate {
    npub: String,
    label: String,
}

/// Cached accounts whose newest kind-0 profile is named `selector`, matching
/// the metadata `name` and `display_name` fields case-insensitively after
/// trimming.
fn cached_profile_candidates(selector: &str, events: &[Event]) -> Vec<ProfileCandidate> {
    let target = normalize_profile_name(selector);
    if target.is_empty() {
        return Vec::new();
    }
    // Keep only the NIP-01 winner per pubkey: newest timestamp, then lowest
    // event ID when timestamps tie.
    let mut newest: HashMap<PublicKey, &Event> = HashMap::new();
    for event in events.iter().filter(|event| event.kind == Kind::Metadata) {
        match newest.entry(event.pubkey) {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                if event.created_at > held.get().created_at
                    || (event.created_at == held.get().created_at && event.id < held.get().id)
                {
                    held.insert(event);
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(event);
            }
        }
    }
    let mut candidates = Vec::new();
    for event in newest.into_values() {
        let Ok(metadata) = Metadata::from_json(&event.content) else {
            continue;
        };
        let names = [metadata.name.as_deref(), metadata.display_name.as_deref()];
        if !names
            .iter()
            .flatten()
            .any(|name| normalize_profile_name(name) == target)
        {
            continue;
        }
        let npub = event
            .pubkey
            .to_bech32()
            // nostr declares `Err = Infallible` for this impl.
            .unwrap_or_default();
        let label = names
            .iter()
            .flatten()
            .map(|name| name.trim())
            .find(|name| !name.is_empty())
            .map_or_else(|| npub.clone(), std::string::ToString::to_string);
        candidates.push(ProfileCandidate { npub, label });
    }
    candidates.sort_by(|a, b| a.npub.cmp(&b.npub));
    candidates
}

fn normalize_profile_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Resolve stored signer material for one expected npub.
///
/// `Ok(None)` means every source reported the entry as missing. Broken or
/// unavailable sources return `Err` instead, so a damaged credential entry is
/// reported rather than silently skipped.
fn resolve_signer_for_npub(
    git_repo: &Option<&Repo>,
    expected_npub: &str,
    password: &Option<String>,
) -> Result<Option<SignerInfo>> {
    let mut store_error = None;

    match credential_store::retrieve_from(expected_npub, credential_store::Backend::Os) {
        Ok(keys) => {
            return Ok(Some(SignerInfo::Nsec {
                nsec: keys.secret_key().to_bech32()?,
                password: password.clone(),
                npub: Some(expected_npub.to_string()),
                verify_npub: true,
            }));
        }
        Err(error) => match &error {
            credential_store::LookupError::Missing(_) => {}
            credential_store::LookupError::Unavailable(_) => {
                store_error = Some(anyhow::Error::new(error));
            }
            credential_store::LookupError::Invalid(_) => {
                return Err(anyhow::Error::new(error));
            }
        },
    }

    match credential_store::retrieve_from(expected_npub, credential_store::Backend::File) {
        Ok(keys) => {
            return Ok(Some(SignerInfo::Nsec {
                nsec: keys.secret_key().to_bech32()?,
                password: password.clone(),
                npub: Some(expected_npub.to_string()),
                verify_npub: true,
            }));
        }
        Err(error) => match &error {
            credential_store::LookupError::Missing(_) => {}
            credential_store::LookupError::Unavailable(_) => {
                store_error = Some(anyhow::Error::new(error));
            }
            credential_store::LookupError::Invalid(_) => {
                return Err(anyhow::Error::new(error));
            }
        },
    }

    // Every nsec source is considered before any bunker source. A matching
    // but unusable nsec profile fails instead of silently switching methods.
    for scope in selection_scopes(git_repo) {
        let Some(nsec) = matching_nsec_config(git_repo, scope, expected_npub)? else {
            continue;
        };
        return Ok(Some(SignerInfo::Nsec {
            nsec: resolve_scope_secret(git_repo, scope, &nsec, true)?,
            password: password.clone(),
            npub: Some(expected_npub.to_string()),
            verify_npub: true,
        }));
    }

    match credential_store::retrieve_bunker_signer_from(
        expected_npub,
        credential_store::Backend::Os,
    ) {
        Ok(record) => {
            return Ok(Some(SignerInfo::Bunker {
                bunker_uri: record.bunker_uri,
                bunker_app_key: record.client_nsec,
                npub: Some(expected_npub.to_string()),
            }));
        }
        Err(error) => match &error {
            credential_store::LookupError::Missing(_) => {}
            credential_store::LookupError::Unavailable(_) => {
                store_error = Some(anyhow::Error::new(error));
            }
            credential_store::LookupError::Invalid(_) => {
                return Err(anyhow::Error::new(error));
            }
        },
    }

    match credential_store::retrieve_bunker_signer_from(
        expected_npub,
        credential_store::Backend::File,
    ) {
        Ok(record) => {
            return Ok(Some(SignerInfo::Bunker {
                bunker_uri: record.bunker_uri,
                bunker_app_key: record.client_nsec,
                npub: Some(expected_npub.to_string()),
            }));
        }
        Err(error) => match &error {
            credential_store::LookupError::Missing(_) => {}
            credential_store::LookupError::Unavailable(_) => {
                store_error = Some(anyhow::Error::new(error));
            }
            credential_store::LookupError::Invalid(_) => {
                return Err(anyhow::Error::new(error));
            }
        },
    }

    // Legacy flat bunker fields remain readable, but a profile is only a
    // candidate when all of its fields come from the same scope.
    for scope in selection_scopes(git_repo) {
        if config_value(git_repo, scope, "nostr.npub")?.as_deref() != Some(expected_npub) {
            continue;
        }
        let Some(bunker_uri) = config_value(git_repo, scope, "nostr.bunker-uri")? else {
            continue;
        };
        let app_key =
            config_value(git_repo, scope, "nostr.bunker-app-key")?.with_context(|| {
                format!(
                    "{} git config has a matching bunker URI but no bunker app key",
                    scope.label()
                )
            })?;
        return Ok(Some(SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key: resolve_scope_secret(git_repo, scope, &app_key, false)?,
            npub: Some(expected_npub.to_string()),
        }));
    }

    if let Some(error) = store_error.take() {
        return Err(error.context(format!(
            "failed to resolve bunker record for selected signer {expected_npub}"
        )));
    }

    Ok(None)
}

fn resolve_selector_npub(git_repo: &Option<&Repo>, selector: &str) -> Result<String> {
    if selector.starts_with("npub1") {
        return PublicKey::parse(selector)
            .context("--signer contains an invalid npub")?
            .to_bech32()
            .map_err(Into::into);
    }
    let alias = credential_store::normalize_alias(selector)?;
    let key = format!("nostr.signer-alias.{alias}");
    let mut store_error = None;
    match credential_store::retrieve_alias_from(&alias, credential_store::Backend::Os) {
        Ok(npub) => return Ok(npub),
        Err(credential_store::LookupError::Missing(_)) => {}
        Err(credential_store::LookupError::Unavailable(error)) => {
            store_error = Some(error);
        }
        Err(error @ credential_store::LookupError::Invalid(_)) => {
            return Err(anyhow::Error::new(error));
        }
    }
    match credential_store::retrieve_alias_from(&alias, credential_store::Backend::File) {
        Ok(npub) => return Ok(npub),
        Err(credential_store::LookupError::Missing(_)) => {}
        Err(credential_store::LookupError::Unavailable(error)) => {
            store_error = Some(error);
        }
        Err(error @ credential_store::LookupError::Invalid(_)) => {
            return Err(anyhow::Error::new(error));
        }
    }
    for scope in selection_scopes(git_repo) {
        if let Some(npub) = config_value(git_repo, scope, &key)? {
            return PublicKey::parse(&npub)
                .with_context(|| {
                    format!(
                        "{} git config maps alias '{alias}' to an invalid npub",
                        scope.label()
                    )
                })?
                .to_bech32()
                .map_err(Into::into);
        }
    }
    store_error.map_or_else(
        || Err(anyhow::Error::new(SignerAliasNotFound { alias })),
        |error| {
            Err(anyhow::Error::new(
                credential_store::LookupError::Unavailable(error),
            ))
        },
    )
}

fn matching_nsec_config(
    git_repo: &Option<&Repo>,
    scope: ConfigScope,
    expected_npub: &str,
) -> Result<Option<String>> {
    let Some(nsec) = config_value(git_repo, scope, "nostr.nsec")? else {
        return Ok(None);
    };
    if config_value(git_repo, scope, "nostr.npub")?.as_deref() == Some(expected_npub)
        || credential_store::parse_pointer(&nsec) == Some(expected_npub)
    {
        return Ok(Some(nsec));
    }
    if let Ok(keys) = nostr::prelude::Keys::parse(&nsec) {
        if keys.public_key().to_bech32().as_deref() == Ok(expected_npub) {
            return Ok(Some(nsec));
        }
    }
    Ok(None)
}

fn resolve_config_secret(
    git_repo: &Option<&Repo>,
    value: &str,
    is_nsec: bool,
    hint_if_plaintext: bool,
) -> Result<String> {
    let classified = if is_nsec {
        credential_store::classify_nsec(value)
    } else {
        credential_store::classify_app_key(value)
    };
    match classified {
        credential_store::ConfigSecret::Encrypted(value) => Ok(value.to_string()),
        credential_store::ConfigSecret::Pointer(name) => {
            let keys = credential_store::retrieve(name)?;
            if is_nsec {
                keys.secret_key().to_bech32().map_err(Into::into)
            } else {
                Ok(keys.secret_key().to_secret_hex())
            }
        }
        credential_store::ConfigSecret::Plaintext(value) => {
            if hint_if_plaintext {
                hint_plaintext_secret(git_repo);
            }
            Ok(value.to_string())
        }
    }
}

/// Plaintext git-config secrets are read as-is and never auto-migrated: a
/// read path that rewrites credential storage nags on every command when no
/// store is available and can strand the only copy of a key inside a
/// sandboxed environment's store. Interactive users get a once-per-run nudge
/// towards `ngit account login` instead; system-config and CLI-supplied
/// secrets never hint.
fn hint_plaintext_secret(git_repo: &Option<&Repo>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HINTED: AtomicBool = AtomicBool::new(false);
    if Interactor::is_non_interactive()
        || credential_store::policy(git_repo) == credential_store::SecretStorage::GitConfig
        || HINTED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    eprintln!(
        "hint: this login's nostr secret is stored in plaintext in git config; run `ngit account login` to move it into a credential store, or set `git config --global nostr.secret-storage git-config` to keep it there and silence this hint."
    );
}

async fn get_signer(
    signer_info: &SignerInfo,
    prompt_for_ncryptsec_password: bool,
) -> Result<(Arc<crate::NgitSigner>, PublicKey)> {
    match signer_info {
        SignerInfo::Nsec {
            nsec,
            password,
            npub,
            verify_npub,
        } => {
            let keys = if nsec.contains("ncryptsec") {
                // TODO get user details from npub
                // TODO add retry loop
                // TODO in retry loop give option to login again
                let password = if let Some(password) = password {
                    password.clone()
                } else {
                    if !prompt_for_ncryptsec_password {
                        bail!(
                            "failed to login without prompts a nsec is encrypted with a password"
                        );
                    }
                    Interactor::default()
                        .password(PromptPasswordParms::default().with_prompt("password"))
                        .context("failed to get password input from interactor.password")?
                };
                decrypt_key(nsec, password.clone().as_str())
                    .context("failed to decrypt key with provided password")
                    .context("failed to decrypt ncryptsec supplied as nsec with password")?
            } else {
                nostr::prelude::Keys::from_str(nsec).context("invalid nsec parameter")?
            };
            let public_key = keys.public_key();
            if *verify_npub {
                let expected = npub
                    .as_deref()
                    .context("selected nsec has no expected npub")?;
                let expected = PublicKey::parse(expected).context("invalid configured npub")?;
                if public_key != expected {
                    bail!("selected nsec belongs to a different npub");
                }
            }
            Ok((Arc::new(crate::NgitSigner::Keys(keys)), public_key))
        }
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            npub,
        } => {
            let uri = NostrConnectUri::parse(bunker_uri)?;
            let s = NostrConnect::new(
                uri,
                nostr::prelude::Keys::from_str(bunker_app_key).context("invalid app key")?,
                Duration::from_secs(10 * 60),
                None,
            )?;
            if let Some(public_key) = npub.clone().and_then(|npub| PublicKey::parse(&npub).ok()) {
                // This key was learned during initial pairing and persisted
                // with the connection. Seed NostrConnect's cache so normal
                // commands do not prompt for a redundant identity request.
                // NgitSigner validates every real signed response instead.
                s.non_secure_set_user_public_key(public_key)?;
                let signer = Arc::new(crate::NgitSigner::Connect(Arc::new(s)));
                Ok((signer, public_key))
            } else {
                let signer = Arc::new(crate::NgitSigner::Connect(Arc::new(s)));
                let term = console::Term::stderr();
                term.write_line("connecting to remote signer...")?;
                let public_key = fetch_public_key(&signer).await?;
                term.clear_last_lines(1)?;
                Ok((signer, public_key))
            }
        }
        SignerInfo::Selection { .. } => {
            bail!("internal error: unresolved signer selection reached signer construction")
        }
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Timestamp, ToBech32, event::FinalizeEvent};

    use super::*;
    use crate::git::{Repo, RepoActions, test_helpers::GitTestRepo};

    #[tokio::test]
    async fn local_alias_resolves_matching_nsec() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        repo.save_git_config_item("nostr.signer-alias.fred", &npub, false)?;
        repo.save_git_config_item("nostr.npub", &npub, false)?;
        repo.save_git_config_item("nostr.nsec", &keys.secret_key().to_bech32()?, false)?;

        let selection = SignerInfo::Selection {
            selector: "fred".to_string(),
        };
        let (info, source, alias) = get_signer_info(
            &Some(&repo),
            &Some(selection),
            &None,
            &Some(SignerInfoSource::CommandLineArguments),
        )
        .await?;
        assert_eq!(source, SignerInfoSource::CommandLineArguments);
        assert_eq!(alias.as_deref(), Some("fred"));
        assert!(matches!(
            info,
            SignerInfo::Nsec { npub: Some(selected), .. } if selected == npub
        ));
        Ok(())
    }

    #[tokio::test]
    async fn alias_mapping_does_not_accept_a_different_nsec() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let expected = Keys::generate().public_key().to_bech32()?;
        let other = Keys::generate();
        repo.save_git_config_item("nostr.signer-alias.fred", &expected, false)?;
        repo.save_git_config_item("nostr.npub", &other.public_key().to_bech32()?, false)?;
        repo.save_git_config_item("nostr.nsec", &other.secret_key().to_bech32()?, false)?;

        let error = resolve_selection(&Some(&repo), "fred", &None, true)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("not available") || message.contains("credential store"),
            "unexpected error: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn malformed_selected_alias_fails_closed() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        repo.save_git_config_item("nostr.signer", "not/a/portable-alias", false)?;
        repo.save_git_config_item(
            "nostr.nsec",
            &Keys::generate().secret_key().to_bech32()?,
            false,
        )?;

        let error = get_signer_info(&Some(&repo), &None, &None, &None)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("signer alias"));
        Ok(())
    }

    #[tokio::test]
    async fn nsec_precedes_bunker_for_same_selected_npub() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        repo.save_git_config_item("nostr.npub", &npub, false)?;
        repo.save_git_config_item("nostr.nsec", &keys.secret_key().to_bech32()?, false)?;
        repo.save_git_config_item(
            "nostr.bunker-uri",
            &format!("bunker://{}", Keys::generate().public_key()),
            false,
        )?;
        repo.save_git_config_item(
            "nostr.bunker-app-key",
            &Keys::generate().secret_key().to_bech32()?,
            false,
        )?;

        let info = resolve_selection(&Some(&repo), &npub, &None, false)
            .await?
            .signer_info;
        assert!(matches!(info, SignerInfo::Nsec { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn configured_nsec_npub_is_verified_on_construction() -> Result<()> {
        let info = SignerInfo::Nsec {
            nsec: Keys::generate().secret_key().to_bech32()?,
            password: None,
            npub: Some(Keys::generate().public_key().to_bech32()?),
            verify_npub: true,
        };
        let error = get_signer(&info, false).await.unwrap_err();
        assert!(error.to_string().contains("different npub"));
        Ok(())
    }

    fn profile_event(
        keys: &Keys,
        name: Option<&str>,
        display_name: Option<&str>,
        created_at: u64,
    ) -> Result<Event> {
        let mut metadata = Metadata::new();
        if let Some(name) = name {
            metadata = metadata.name(name);
        }
        if let Some(display_name) = display_name {
            metadata = metadata.display_name(display_name);
        }
        Ok(EventBuilder::new(Kind::Metadata, metadata.as_json())
            .custom_created_at(Timestamp::from(created_at))
            .finalize(keys)?)
    }

    fn nsec_info(keys: &Keys) -> Result<SignerInfo> {
        Ok(SignerInfo::Nsec {
            nsec: keys.secret_key().to_bech32()?,
            password: None,
            npub: Some(keys.public_key().to_bech32()?),
            verify_npub: true,
        })
    }

    #[test]
    fn profile_names_match_case_insensitively_after_trimming() -> Result<()> {
        let keys = Keys::generate();
        let named = vec![profile_event(&keys, Some("DanConwayDev"), None, 10)?];
        assert_eq!(
            cached_profile_candidates("  danconwaydev ", &named).len(),
            1
        );
        assert!(cached_profile_candidates("danconway", &named).is_empty());
        let displayed = vec![profile_event(&keys, None, Some("Dan's Agent"), 10)?];
        assert_eq!(
            cached_profile_candidates("dan's agent", &displayed).len(),
            1
        );
        assert!(cached_profile_candidates("", &displayed).is_empty());
        Ok(())
    }

    #[test]
    fn only_the_newest_cached_profile_per_account_is_matched() -> Result<()> {
        let keys = Keys::generate();
        let events = vec![
            profile_event(&keys, Some("old-name"), None, 10)?,
            profile_event(&keys, Some("new-name"), None, 20)?,
        ];
        assert!(cached_profile_candidates("old-name", &events).is_empty());
        assert_eq!(cached_profile_candidates("new-name", &events).len(), 1);
        Ok(())
    }

    #[test]
    fn equal_timestamp_profiles_use_the_lower_event_id() -> Result<()> {
        let keys = Keys::generate();
        let first = profile_event(&keys, Some("first-name"), None, 10)?;
        let second = profile_event(&keys, Some("second-name"), None, 10)?;
        let (winner_name, loser_name) = if first.id < second.id {
            ("first-name", "second-name")
        } else {
            ("second-name", "first-name")
        };
        let events = vec![first, second];

        assert_eq!(cached_profile_candidates(winner_name, &events).len(), 1);
        assert!(cached_profile_candidates(loser_name, &events).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn alias_mapping_beats_cached_profile_name() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        repo.save_git_config_item("nostr.signer-alias.fred", &npub, false)?;
        repo.save_git_config_item("nostr.npub", &npub, false)?;
        repo.save_git_config_item("nostr.nsec", &keys.secret_key().to_bech32()?, false)?;

        // an existing alias resolves without consulting cached profiles, so a
        // same-named profile for another account can never shadow it
        let resolved = resolve_selection(&Some(&repo), "fred", &None, true).await?;
        assert_eq!(resolved.npub, npub);
        assert_eq!(resolved.alias.as_deref(), Some("fred"));
        Ok(())
    }

    #[test]
    fn only_name_shaped_alias_failures_trigger_profile_name_lookup() {
        let alias_not_found = anyhow::Error::new(SignerAliasNotFound {
            alias: "fred".to_string(),
        });
        assert!(selector_may_be_profile_name("fred", &alias_not_found));
        // selectors that can never be alias tokens fall through regardless of
        // the reported alias error
        assert!(selector_may_be_profile_name(
            "DanConwayDev's Agent",
            &anyhow::anyhow!("signer alias must start with a letter")
        ));
        // an unavailable alias store keeps alias resolution authoritative
        assert!(!selector_may_be_profile_name(
            "fred",
            &anyhow::Error::new(credential_store::LookupError::Unavailable(anyhow::anyhow!(
                "store down"
            )))
        ));
        // an invalid npub is an npub error, never a profile name
        assert!(!selector_may_be_profile_name(
            "npub1notavalidkey",
            &anyhow::anyhow!("--signer contains an invalid npub")
        ));
    }

    #[test]
    fn uncredentialed_same_named_profile_is_filtered_out() -> Result<()> {
        let credentialed = Keys::generate();
        let squatter = Keys::generate();
        let npub = credentialed.public_key().to_bech32()?;
        let events = vec![
            profile_event(&credentialed, Some("Shared Name"), None, 10)?,
            profile_event(&squatter, Some("shared name"), None, 20)?,
        ];
        let resolved = select_credentialed_profile("Shared Name", &events, |candidate| {
            Ok(if candidate == npub {
                Some(nsec_info(&credentialed)?)
            } else {
                None
            })
        })?;
        assert_eq!(resolved.npub, npub);
        assert!(resolved.alias.is_none());
        Ok(())
    }

    #[test]
    fn ambiguous_credentialed_profile_name_fails_closed() -> Result<()> {
        let first = Keys::generate();
        let second = Keys::generate();
        let events = vec![
            profile_event(&first, Some("Shared Name"), None, 10)?,
            profile_event(&second, Some("Shared Name"), None, 20)?,
        ];
        let error = select_credentialed_profile("Shared Name", &events, |candidate| {
            Ok(Some(SignerInfo::Nsec {
                nsec: "unused".to_string(),
                password: None,
                npub: Some(candidate.to_string()),
                verify_npub: true,
            }))
        })
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&first.public_key().to_bech32()?));
        assert!(message.contains(&second.public_key().to_bech32()?));
        assert!(message.contains("--signer <npub>"));
        Ok(())
    }

    #[test]
    fn broken_candidate_credentials_propagate_instead_of_being_skipped() -> Result<()> {
        let broken = Keys::generate();
        let usable = Keys::generate();
        let broken_npub = broken.public_key().to_bech32()?;
        let events = vec![
            profile_event(&broken, Some("Shared Name"), None, 10)?,
            profile_event(&usable, Some("Shared Name"), None, 20)?,
        ];
        let error = select_credentialed_profile("Shared Name", &events, |candidate| {
            if candidate == broken_npub {
                Err(anyhow::Error::new(credential_store::LookupError::Invalid(
                    "corrupt entry".to_string(),
                )))
            } else {
                Ok(Some(nsec_info(&usable)?))
            }
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains(&broken_npub));
        Ok(())
    }

    #[test]
    fn unmatched_profile_names_yield_cache_guidance() -> Result<()> {
        let error = select_credentialed_profile("No Such Name", &[], |_| Ok(None)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("--signer <npub>"), "got: {message}");
        assert!(message.contains("cache"), "got: {message}");

        // a matched profile without stored credentials points at logging in
        let keys = Keys::generate();
        let events = vec![profile_event(&keys, Some("Casper"), None, 10)?];
        let error = select_credentialed_profile("casper", &events, |_| Ok(None)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("--signer <npub>"), "got: {message}");
        assert!(message.contains("log"), "got: {message}");
        Ok(())
    }
}
