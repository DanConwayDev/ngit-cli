use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::OnceLock,
};

use anyhow::{Context, Result, anyhow, bail};
use keyring::Error as KeyringError;
use nostr::prelude::{Keys, PublicKey, ToBech32, nip46::NostrConnectUri};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Keyring service shared by nostr applications, not specific to ngit: the
/// git config keys this pairs with are `nostr.*` rather than `ngit.*`, and
/// an entry is named by the npub of the key it holds, so nothing about an
/// entry is ngit-specific. See `docs/credential-storage.md`.
pub const SERVICE: &str = "nostr";
/// Debug-build-only override: redirects the file store to the given path and
/// disables the OS credential store so tests never touch a real keychain.
pub const FILE_ENV: &str = "NGIT_KEYRING_FILE";
pub const POLICY_ENV: &str = "NGIT_SECRET_STORAGE";
/// Pre-release name of the policy switch. Read (never written) so early
/// adopters who opted out via `NGIT_CREDENTIAL_STORE=false` stay opted out.
pub const LEGACY_POLICY_ENV: &str = "NGIT_CREDENTIAL_STORE";

/// Where `ngit account login` / `account create` store fresh secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStorage {
    /// OS credential store, falling back to ngit's file store.
    Auto,
    /// ngit's file store only.
    File,
    /// Plaintext in git config.
    GitConfig,
}

/// Which backend ended up holding a stored secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Os,
    File,
}

const SIGNER_RECORD_VERSION: u8 = 1;
const ALIAS_RECORD_VERSION: u8 = 1;
const ALIAS_PREFIX: &str = "alias:";
const ALIAS_CREDENTIAL_PREFIX: &str = "alias-credential:";
const ALIAS_DEFAULT_CREDENTIAL: &str = "default";
const SIGNER_PREFIX: &str = "signer:";
const ALIAS_SIGNER_PREFIX: &str = "signer-alias:";

/// Public identities and aliases known to the credential backends. No secret
/// material is exposed through this inventory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CredentialInventory {
    pub accounts: BTreeSet<String>,
    pub aliases: BTreeMap<String, String>,
}

/// A complete credential-store record for a NIP-46 signer. The client key is
/// an application key used to communicate with the bunker, never the user's
/// identity key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BunkerSigner {
    pub npub: String,
    pub bunker_uri: String,
    pub client_nsec: String,
}

/// The identity and optional concrete signer credential selected by an alias.
///
/// Canonical alias entries contain only an npub for compatibility with older
/// clients. A separate public companion entry may bind one particular NIP-46
/// connection when several connections serve the same identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerAlias {
    pub npub: String,
    pub credential: Option<String>,
}

#[derive(Debug)]
struct SignerCredentialConflict {
    npub: String,
}

impl fmt::Display for SignerCredentialConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a different signer credential is already stored for {}; refusing to replace it. Re-run with `--alias <name>` to store this remote signer connection separately",
            self.npub
        )
    }
}

impl std::error::Error for SignerCredentialConflict {}

/// Whether an error reports an intentional refusal to replace a stored signer.
///
/// Callers must not treat this as an unavailable credential backend and offer
/// plaintext storage as a fallback.
pub(crate) fn is_signer_credential_conflict(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SignerCredentialConflict>().is_some()
}

#[derive(Debug, Serialize, Deserialize)]
struct BunkerSignerRecord {
    version: u8,
    #[serde(rename = "type")]
    record_type: String,
    npub: String,
    bunker_uri: String,
    client_nsec: String,
}

/// Read compatibility for builds that briefly wrote the signer binding into
/// the alias value itself. New writes use a raw npub plus a companion entry.
#[derive(Debug, Serialize, Deserialize)]
struct SignerAliasRecord {
    version: u8,
    #[serde(rename = "type")]
    record_type: String,
    npub: String,
    credential: String,
}

static POLICY_OVERRIDE: OnceLock<SecretStorage> = OnceLock::new();

/// Process-wide policy override set from `--secret-storage`. Takes precedence
/// over environment variables and git config.
pub fn set_policy_override(policy: SecretStorage) {
    let _ = POLICY_OVERRIDE.set(policy);
}

pub fn parse_policy(value: &str) -> Option<SecretStorage> {
    match value.to_ascii_lowercase().as_str() {
        // boolean spellings accepted for compatibility with the pre-release
        // `nostr.credential-store` on/off switch
        "auto" | "true" | "yes" | "on" | "1" => Some(SecretStorage::Auto),
        "file" => Some(SecretStorage::File),
        "git-config" | "gitconfig" | "false" | "no" | "off" | "0" => Some(SecretStorage::GitConfig),
        _ => None,
    }
}

/// Resolve the secret-storage policy: CLI override, then env (new name before
/// legacy), then local git config, then global git config, then `Auto`.
pub fn policy(git_repo: &Option<&crate::git::Repo>) -> SecretStorage {
    if let Some(policy) = POLICY_OVERRIDE.get() {
        return *policy;
    }
    for env in [POLICY_ENV, LEGACY_POLICY_ENV] {
        if let Some(policy) = std::env::var(env).ok().as_deref().and_then(parse_policy) {
            return policy;
        }
    }
    if git_repo.is_some() {
        for key in ["nostr.secret-storage", "nostr.credential-store"] {
            if let Some(policy) = crate::git::get_git_config_item(git_repo, key)
                .ok()
                .flatten()
                .as_deref()
                .and_then(parse_policy)
            {
                return policy;
            }
        }
    }
    for key in ["nostr.secret-storage", "nostr.credential-store"] {
        if let Some(policy) = crate::git::get_git_config_item_global(git_repo, key)
            .ok()
            .flatten()
            .as_deref()
            .and_then(parse_policy)
        {
            return policy;
        }
    }
    SecretStorage::Auto
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSecret<'a> {
    Plaintext(&'a str),
    Encrypted(&'a str),
    Pointer(&'a str),
}

#[derive(Debug)]
pub enum LookupError {
    Missing(String),
    Unavailable(anyhow::Error),
    Invalid(String),
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(
                f,
                "credential '{name}' was not found in the credential store"
            ),
            Self::Unavailable(error) => write!(
                f,
                "credential store is unavailable: {error}. Restore access to the OS credential store or ngit's file store, or use --nsec / --bunker-uri with --bunker-app-key for one command"
            ),
            Self::Invalid(error) => write!(f, "invalid credential-store entry: {error}"),
        }
    }
}

impl std::error::Error for LookupError {}

pub fn classify_nsec(value: &str) -> ConfigSecret<'_> {
    if value.starts_with("ncryptsec1") {
        ConfigSecret::Encrypted(value)
    } else if parse_pointer(value).is_some() {
        ConfigSecret::Pointer(value)
    } else {
        ConfigSecret::Plaintext(value)
    }
}

pub fn classify_app_key(value: &str) -> ConfigSecret<'_> {
    if parse_pointer(value).is_some() {
        ConfigSecret::Pointer(value)
    } else {
        ConfigSecret::Plaintext(value)
    }
}

/// A pointer is the entry's npub-based name: either a bare `npub1…` or the
/// legacy `npub1…/<8-alphanumeric>` form written by pre-release versions.
/// Returns the npub the retrieved key must verify against.
pub fn parse_pointer(value: &str) -> Option<&str> {
    let npub = if let Some((npub, suffix)) = value.split_once('/') {
        if suffix.len() != 8 || !suffix.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        npub
    } else {
        value
    };
    if !npub.starts_with("npub1") {
        return None;
    }
    PublicKey::parse(npub).ok()?;
    Some(npub)
}

/// Entry names are the bech32 npub of the stored key itself: stable across
/// logins of the same account (a re-login overwrites the entry with the same
/// secret) and derivable after logout, which lets the logout guidance print
/// a ready-to-paste `forget-keys` command. Sharing one entry per account is
/// safe because logout never deletes entries; bunker app keys are freshly
/// generated per login, so their entries cannot collide either.
pub fn entry_name(keys: &Keys) -> Result<String> {
    keys.public_key().to_bech32().map_err(Into::into)
}

pub fn signer_entry_name(npub: &str) -> Result<String> {
    Ok(format!("{SIGNER_PREFIX}{}", canonical_npub(npub)?))
}

pub fn alias_signer_entry_name(alias: &str) -> Result<String> {
    Ok(format!("{ALIAS_SIGNER_PREFIX}{}", normalize_alias(alias)?))
}

pub fn is_bunker_signer_entry_name(name: &str) -> bool {
    name.strip_prefix(SIGNER_PREFIX)
        .is_some_and(|npub| canonical_npub(npub).is_ok())
        || name
            .strip_prefix(ALIAS_SIGNER_PREFIX)
            .is_some_and(|alias| normalize_alias(alias).is_ok())
}

pub fn alias_entry_name(alias: &str) -> Result<String> {
    Ok(format!("{ALIAS_PREFIX}{}", normalize_alias(alias)?))
}

fn alias_credential_entry_name(alias: &str) -> Result<String> {
    Ok(format!(
        "{ALIAS_CREDENTIAL_PREFIX}{}",
        normalize_alias(alias)?
    ))
}

/// Normalize an alias for both keyring and git-config use. Git variable names
/// are case-insensitive and accept only alphanumeric characters and `-`, so
/// aliases use their common, portable subset.
pub fn normalize_alias(alias: &str) -> Result<String> {
    let alias = alias.trim();
    if alias.is_empty()
        || alias.len() > 64
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !alias
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
    {
        bail!(
            "signer alias must start with a letter and contain at most 64 letters, numbers, or hyphens"
        );
    }
    Ok(alias.to_ascii_lowercase())
}

fn canonical_npub(npub: &str) -> Result<String> {
    if !npub.starts_with("npub1") {
        bail!("signer identity must be an npub");
    }
    PublicKey::parse(npub)
        .context("invalid signer npub")?
        .to_bech32()
        .map_err(Into::into)
}

/// True when the debug-only `NGIT_KEYRING_FILE` redirect is active; the OS
/// credential store is then bypassed entirely so tests can never touch a
/// real keychain.
fn os_store_disabled() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var(FILE_ENV).is_ok()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// Store `keys`, read-back-verified, in the OS credential store — or in
/// ngit's file store when the policy selects it or the OS store fails.
/// Returns the new entry name and the backend that holds it.
pub fn store(keys: &Keys, policy: SecretStorage) -> Result<(String, Backend)> {
    let name = entry_name(keys)?;
    let os_error = if policy == SecretStorage::File || os_store_disabled() {
        None
    } else {
        match store_os(&name, keys) {
            Ok(()) => {
                remember_account_for_listing(&name);
                return Ok((name, Backend::Os));
            }
            Err(error) => Some(anyhow!(error)),
        }
    };
    store_file(&name, keys).with_context(|| match &os_error {
        Some(error) => format!(
            "failed to write ngit's file store after the OS credential store failed: {error:#}"
        ),
        None => "failed to write ngit's file store".to_string(),
    })?;
    remember_account_for_listing(&name);
    Ok((name, Backend::File))
}

/// Store a complete NIP-46 signer record under `signer:<user-npub>` or an
/// alias-specific entry when another credential already owns the identity's
/// default slot.
/// Any one-time pairing secret in the supplied bunker URI is deliberately
/// removed before serialization.
pub fn store_bunker_signer(
    npub: &str,
    bunker_uri: &str,
    client_keys: &Keys,
    alias: Option<&str>,
    policy: SecretStorage,
) -> Result<(String, Backend)> {
    let npub = canonical_npub(npub)?;
    let bunker_uri = sanitized_bunker_uri(bunker_uri)?;
    let record = BunkerSignerRecord {
        version: SIGNER_RECORD_VERSION,
        record_type: "bunker".to_string(),
        npub: npub.clone(),
        bunker_uri,
        client_nsec: client_keys.secret_key().to_bech32()?,
    };
    let serialized = Zeroizing::new(
        serde_json::to_string(&record).context("failed to serialize bunker signer record")?,
    );
    let signer = parse_bunker_signer("new remote signer", &npub, serialized.as_str())?;
    let name = bunker_signer_entry_for_store(&npub, alias, &signer)?;
    let stored = store_value(&name, serialized.as_str(), policy)?;
    remember_account_for_listing(&npub);
    Ok(stored)
}

fn bunker_signer_entry_for_store(
    npub: &str,
    alias: Option<&str>,
    signer: &BunkerSigner,
) -> Result<String> {
    let default_name = signer_entry_name(npub)?;
    let existing_nsec = signer_nsec_exists(npub)?;
    let existing_default = bunker_signers_named(&default_name, npub)?;

    let Some(alias) = alias else {
        if existing_nsec || existing_default.iter().any(|stored| stored != signer) {
            return Err(SignerCredentialConflict {
                npub: npub.to_string(),
            }
            .into());
        }
        return Ok(default_name);
    };

    let alias_name = alias_signer_entry_name(alias)?;
    if !bunker_signers_named(&alias_name, npub)?.is_empty() {
        return Ok(alias_name);
    }
    if existing_nsec || existing_default.iter().any(|stored| stored != signer) {
        Ok(alias_name)
    } else {
        Ok(default_name)
    }
}

fn signer_nsec_exists(npub: &str) -> Result<bool> {
    let mut found = false;
    for backend in [Backend::Os, Backend::File] {
        match retrieve_from(npub, backend) {
            Ok(_) => found = true,
            Err(LookupError::Missing(_) | LookupError::Unavailable(_)) => {}
            Err(error @ LookupError::Invalid(_)) => return Err(anyhow!(error)),
        }
    }
    Ok(found)
}

fn bunker_signers_named(name: &str, npub: &str) -> Result<Vec<BunkerSigner>> {
    let mut signers = Vec::new();
    for backend in [Backend::Os, Backend::File] {
        match retrieve_bunker_signer_named_from(name, npub, backend) {
            Ok(signer) => signers.push(signer),
            Err(LookupError::Missing(_) | LookupError::Unavailable(_)) => {}
            Err(error @ LookupError::Invalid(_)) => return Err(anyhow!(error)),
        }
    }
    Ok(signers)
}

pub fn retrieve_bunker_signer(npub: &str) -> std::result::Result<BunkerSigner, LookupError> {
    let expected_npub = canonical_npub(npub)
        .map_err(|error| LookupError::Invalid(format!("invalid selected signer: {error:#}")))?;
    let name = signer_entry_name(&expected_npub)
        .map_err(|error| LookupError::Invalid(error.to_string()))?;
    let serialized = retrieve_value(&name)?;
    parse_bunker_signer(&name, &expected_npub, &serialized)
}

pub fn retrieve_bunker_signer_named(
    name: &str,
    expected_npub: &str,
) -> std::result::Result<BunkerSigner, LookupError> {
    let expected_npub = canonical_npub(expected_npub)
        .map_err(|error| LookupError::Invalid(format!("invalid selected signer: {error:#}")))?;
    if !valid_bunker_signer_entry_name(name, &expected_npub) {
        return Err(LookupError::Invalid(format!(
            "credential '{name}' is not a signer entry for {expected_npub}"
        )));
    }
    let serialized = retrieve_value(name)?;
    parse_bunker_signer(name, &expected_npub, &serialized)
}

/// Alias-specific bunker credentials for one identity, with identical
/// connections deduplicated. This supplies the npub fallback when no default
/// credential exists, despite platform keyrings not supporting enumeration.
pub fn alias_bunker_signers(npub: &str) -> Result<Vec<(String, BunkerSigner)>> {
    let npub = canonical_npub(npub)?;
    let mut signers = Vec::new();
    for (alias, alias_npub) in inventory()?.aliases {
        if alias_npub != npub {
            continue;
        }
        let name = alias_signer_entry_name(&alias)?;
        match retrieve_bunker_signer_named(&name, &npub) {
            Ok(signer) if signers.iter().any(|(_, existing)| existing == &signer) => {}
            Ok(signer) => signers.push((alias, signer)),
            Err(LookupError::Missing(_)) => {}
            Err(error @ (LookupError::Unavailable(_) | LookupError::Invalid(_))) => {
                return Err(anyhow!(error));
            }
        }
    }
    Ok(signers)
}

fn retrieve_bunker_signer_named_from(
    name: &str,
    expected_npub: &str,
    backend: Backend,
) -> std::result::Result<BunkerSigner, LookupError> {
    let expected_npub = canonical_npub(expected_npub)
        .map_err(|error| LookupError::Invalid(format!("invalid selected signer: {error:#}")))?;
    if !valid_bunker_signer_entry_name(name, &expected_npub) {
        return Err(LookupError::Invalid(format!(
            "credential '{name}' is not a signer entry for {expected_npub}"
        )));
    }
    let serialized = retrieve_value_from(name, backend)?;
    parse_bunker_signer(name, &expected_npub, &serialized)
}

pub fn retrieve_bunker_signer_from(
    npub: &str,
    backend: Backend,
) -> std::result::Result<BunkerSigner, LookupError> {
    let expected_npub = canonical_npub(npub)
        .map_err(|error| LookupError::Invalid(format!("invalid selected signer: {error:#}")))?;
    let name = signer_entry_name(&expected_npub)
        .map_err(|error| LookupError::Invalid(error.to_string()))?;
    let signer = retrieve_bunker_signer_named_from(&name, &expected_npub, backend)?;
    if backend == Backend::Os {
        remember_account_for_listing(&expected_npub);
    }
    Ok(signer)
}

pub fn store_alias(
    alias: &str,
    npub: &str,
    credential: Option<&str>,
    policy: SecretStorage,
) -> Result<(String, Backend)> {
    ensure_alias_available(alias, npub)?;
    let name = alias_entry_name(alias)?;
    let npub = canonical_npub(npub)?;
    let credential_name = alias_credential_entry_name(alias)?;
    let credential = if let Some(credential) = credential {
        validate_alias_credential(alias, &npub, credential)?;
        credential
    } else {
        ALIAS_DEFAULT_CREDENTIAL
    };
    store_value(&credential_name, credential, policy)
        .context("failed to store the signer alias credential binding")?;
    // Keep this value as a raw npub: released clients parse it directly.
    let stored = store_value(&name, &npub, policy)?;
    remember_alias_for_listing(alias, &npub);
    Ok(stored)
}

/// Credential-store aliases are machine-wide because they take precedence
/// over every Git-config scope. Refuse to shadow an existing mapping with a
/// different identity; Git-only aliases remain free to vary between repos.
pub fn ensure_alias_available(alias: &str, npub: &str) -> Result<()> {
    let alias = normalize_alias(alias)?;
    let npub = canonical_npub(npub)?;
    for backend in [Backend::Os, Backend::File] {
        match retrieve_signer_alias_from(&alias, backend) {
            Ok(existing) if existing.npub == npub => {}
            Ok(existing) => {
                bail!(
                    "signer alias '{alias}' already identifies {} in the {} credential store; choose another alias or remove entry 'alias:{alias}' first",
                    existing.npub,
                    match backend {
                        Backend::Os => "OS",
                        Backend::File => "file",
                    }
                );
            }
            // An unavailable OS store must not disable the documented file
            // fallback. If it later recovers, normal read precedence applies.
            Err(LookupError::Missing(_) | LookupError::Unavailable(_)) => {}
            Err(error @ LookupError::Invalid(_)) => return Err(anyhow!(error)),
        }
    }
    Ok(())
}

pub fn retrieve_alias(alias: &str) -> std::result::Result<String, LookupError> {
    retrieve_signer_alias(alias).map(|alias| alias.npub)
}

pub fn retrieve_signer_alias(alias: &str) -> std::result::Result<SignerAlias, LookupError> {
    let name = alias_entry_name(alias)
        .map_err(|error| LookupError::Invalid(format!("invalid signer alias: {error:#}")))?;
    let value = retrieve_value(&name)?;
    let target = parse_signer_alias(&name, &value)?;
    attach_alias_credential(alias, target)
}

pub fn retrieve_alias_from(
    alias: &str,
    backend: Backend,
) -> std::result::Result<String, LookupError> {
    retrieve_signer_alias_from(alias, backend).map(|alias| alias.npub)
}

pub fn retrieve_signer_alias_from(
    alias: &str,
    backend: Backend,
) -> std::result::Result<SignerAlias, LookupError> {
    let name = alias_entry_name(alias)
        .map_err(|error| LookupError::Invalid(format!("invalid signer alias: {error:#}")))?;
    let value = retrieve_value_from(&name, backend)?;
    let target = attach_alias_credential(alias, parse_signer_alias(&name, &value)?)?;
    if backend == Backend::Os {
        remember_account_for_listing(&target.npub);
        remember_alias_for_listing(alias, &target.npub);
    }
    Ok(target)
}

fn attach_alias_credential(
    alias: &str,
    mut target: SignerAlias,
) -> std::result::Result<SignerAlias, LookupError> {
    // Typed values emitted by the short-lived intermediate format already
    // carry an exact binding. Reading them lets the next login migrate them.
    if target.credential.is_some() {
        return Ok(target);
    }
    let name = alias_credential_entry_name(alias)
        .map_err(|error| LookupError::Invalid(format!("invalid signer alias: {error:#}")))?;
    for backend in [Backend::Os, Backend::File] {
        match retrieve_value_from(&name, backend) {
            Ok(credential) if credential == ALIAS_DEFAULT_CREDENTIAL => break,
            Ok(credential) => {
                validate_alias_credential(alias, &target.npub, &credential)
                    .map_err(|error| LookupError::Invalid(error.to_string()))?;
                target.credential = Some(credential);
                break;
            }
            // The companion is optional for aliases created by older clients.
            // An unavailable OS store must not hide a usable file-store alias.
            Err(LookupError::Missing(_) | LookupError::Unavailable(_)) => {}
            Err(error @ LookupError::Invalid(_)) => return Err(error),
        }
    }
    Ok(target)
}

fn parse_signer_alias(name: &str, value: &str) -> std::result::Result<SignerAlias, LookupError> {
    if !value.trim_start().starts_with('{') {
        return canonical_alias_npub(name, value).map(|npub| SignerAlias {
            npub,
            credential: None,
        });
    }
    let record: SignerAliasRecord = serde_json::from_str(value).map_err(|error| {
        LookupError::Invalid(format!(
            "credential '{name}' is not a valid signer alias record: {error}"
        ))
    })?;
    if record.version != ALIAS_RECORD_VERSION || record.record_type != "alias" {
        return Err(LookupError::Invalid(format!(
            "credential '{name}' uses an unsupported signer alias record type or version"
        )));
    }
    let npub = canonical_alias_npub(name, &record.npub)?;
    let alias = name.strip_prefix(ALIAS_PREFIX).ok_or_else(|| {
        LookupError::Invalid(format!("credential '{name}' is not a signer alias"))
    })?;
    validate_alias_credential(alias, &npub, &record.credential)
        .map_err(|error| LookupError::Invalid(error.to_string()))?;
    Ok(SignerAlias {
        npub,
        credential: Some(record.credential),
    })
}

fn validate_alias_credential(alias: &str, npub: &str, credential: &str) -> Result<()> {
    let default = signer_entry_name(npub)?;
    let dedicated = alias_signer_entry_name(alias)?;
    if credential != default && credential != dedicated {
        bail!("signer alias '{alias}' points to invalid credential entry '{credential}'");
    }
    Ok(())
}

fn canonical_alias_npub(name: &str, npub: &str) -> std::result::Result<String, LookupError> {
    canonical_npub(npub).map_err(|error| {
        LookupError::Invalid(format!(
            "credential '{name}' contains an invalid npub: {error:#}"
        ))
    })
}

fn sanitized_bunker_uri(value: &str) -> Result<String> {
    match NostrConnectUri::parse(value).context("invalid bunker URI in signer record")? {
        NostrConnectUri::Bunker {
            remote_signer_public_key,
            relays,
            secret: _,
        } => Ok(NostrConnectUri::Bunker {
            remote_signer_public_key,
            relays,
            secret: None,
        }
        .to_string()),
        NostrConnectUri::Client { .. } => bail!("signer record requires a bunker:// URI"),
    }
}

fn parse_bunker_signer(
    name: &str,
    expected_npub: &str,
    serialized: &str,
) -> std::result::Result<BunkerSigner, LookupError> {
    let record: BunkerSignerRecord = serde_json::from_str(serialized).map_err(|error| {
        LookupError::Invalid(format!(
            "credential '{name}' is not a valid signer record: {error}"
        ))
    })?;
    if record.version != SIGNER_RECORD_VERSION || record.record_type != "bunker" {
        return Err(LookupError::Invalid(format!(
            "credential '{name}' uses an unsupported signer record type or version"
        )));
    }
    let npub = canonical_npub(&record.npub).map_err(|error| {
        LookupError::Invalid(format!(
            "credential '{name}' contains an invalid npub: {error:#}"
        ))
    })?;
    if npub != expected_npub {
        return Err(LookupError::Invalid(format!(
            "credential '{name}' names a different signer"
        )));
    }
    let bunker_uri = sanitized_bunker_uri(&record.bunker_uri).map_err(|error| {
        LookupError::Invalid(format!(
            "credential '{name}' contains an invalid bunker URI: {error:#}"
        ))
    })?;
    if bunker_uri != record.bunker_uri {
        return Err(LookupError::Invalid(format!(
            "credential '{name}' contains a one-time bunker secret"
        )));
    }
    let client_keys = Keys::parse(&record.client_nsec).map_err(|_| {
        LookupError::Invalid(format!(
            "credential '{name}' contains an invalid client nsec"
        ))
    })?;
    Ok(BunkerSigner {
        npub,
        bunker_uri,
        client_nsec: client_keys.secret_key().to_bech32().map_err(|error| {
            LookupError::Invalid(format!("failed to encode client nsec: {error}"))
        })?,
    })
}

fn valid_bunker_signer_entry_name(name: &str, expected_npub: &str) -> bool {
    signer_entry_name(expected_npub).is_ok_and(|expected| expected == name)
        || name
            .strip_prefix(ALIAS_SIGNER_PREFIX)
            .is_some_and(|alias| normalize_alias(alias).is_ok())
}

fn store_value(name: &str, value: &str, policy: SecretStorage) -> Result<(String, Backend)> {
    if policy == SecretStorage::File && !os_store_disabled() {
        match os_store::get_value(name) {
            Ok(existing) => ensure_unshadowed_file_value(name, value, Some(&existing))?,
            Err(OsError::NoEntry | OsError::Store(_)) => {}
            Err(OsError::Corrupt) => {
                bail!(
                    "OS credential '{name}' contains invalid data and would shadow the new file-store entry; remove it before using --secret-storage file"
                );
            }
        }
    }
    let os_error = if policy == SecretStorage::File || os_store_disabled() {
        None
    } else {
        match os_store::set_value(name, value) {
            Ok(()) => match os_store::get_value(name) {
                Ok(retrieved) if retrieved == value => {
                    return Ok((name.to_string(), Backend::Os));
                }
                Ok(_) => {
                    let _ = os_store::delete(name);
                    Some(anyhow!(
                        "OS credential store read-back returned different data"
                    ))
                }
                Err(error) => {
                    let _ = os_store::delete(name);
                    Some(anyhow!(error).context("failed to verify OS credential store write"))
                }
            },
            Err(error) => Some(anyhow!(error)),
        }
    };
    file_store::set_value(name, value).with_context(|| match &os_error {
        Some(error) => format!(
            "failed to write ngit's file store after the OS credential store failed: {error:#}"
        ),
        None => "failed to write ngit's file store".to_string(),
    })?;
    if file_store::get_value(name)?.as_deref() != Some(value) {
        let _ = file_store::delete(name);
        bail!("file-store read-back verification failed");
    }
    Ok((name.to_string(), Backend::File))
}

fn ensure_unshadowed_file_value(name: &str, value: &str, existing: Option<&str>) -> Result<()> {
    if existing.is_some_and(|existing| existing != value) {
        bail!(
            "OS credential '{name}' contains different data and would shadow the new file-store entry; remove it with `ngit account forget-keys {name}` before using --secret-storage file"
        );
    }
    Ok(())
}

fn retrieve_value(name: &str) -> std::result::Result<String, LookupError> {
    let os_error = match retrieve_value_from(name, Backend::Os) {
        Ok(value) => return Ok(value),
        Err(LookupError::Missing(_)) => None,
        Err(LookupError::Unavailable(error)) => Some(error),
        Err(error @ LookupError::Invalid(_)) => return Err(error),
    };
    match retrieve_value_from(name, Backend::File) {
        Ok(value) => Ok(value),
        Err(LookupError::Missing(_)) => os_error.map_or_else(
            || Err(LookupError::Missing(name.to_string())),
            |error| Err(LookupError::Unavailable(error)),
        ),
        Err(LookupError::Unavailable(error)) => Err(LookupError::Unavailable(match os_error {
            Some(os_error) => {
                error.context(format!("OS credential store also failed: {os_error:#}"))
            }
            None => error,
        })),
        Err(error @ LookupError::Invalid(_)) => Err(error),
    }
}

fn retrieve_value_from(name: &str, backend: Backend) -> std::result::Result<String, LookupError> {
    match backend {
        Backend::Os if os_store_disabled() => Err(LookupError::Missing(name.to_string())),
        Backend::Os => match os_store::get_value(name) {
            Ok(value) => Ok(value),
            Err(OsError::NoEntry) => Err(LookupError::Missing(name.to_string())),
            Err(OsError::Corrupt) => Err(LookupError::Invalid(format!(
                "OS credential '{name}' contains invalid text"
            ))),
            Err(error @ OsError::Store(_)) => Err(LookupError::Unavailable(anyhow!(error))),
        },
        Backend::File => match file_store::get_value(name) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(LookupError::Missing(name.to_string())),
            Err(error) => Err(LookupError::Unavailable(error)),
        },
    }
}

fn store_os(name: &str, keys: &Keys) -> Result<()> {
    os_store::set(name, keys).context("failed to write OS credential store")?;
    match os_store::get(name) {
        Ok(retrieved) if retrieved.public_key() == keys.public_key() => Ok(()),
        Ok(_) => {
            let _ = os_store::delete(name);
            bail!("OS credential store read-back returned a different key")
        }
        Err(error) => {
            let _ = os_store::delete(name);
            Err(anyhow!(error).context("failed to verify OS credential store write"))
        }
    }
}

fn store_file(name: &str, keys: &Keys) -> Result<()> {
    file_store::set(name, keys)?;
    match file_store::get(name)? {
        Some(retrieved) if retrieved.public_key() == keys.public_key() => Ok(()),
        _ => {
            let _ = file_store::delete(name);
            bail!("file store read-back verification failed")
        }
    }
}

/// Location of ngit's file store, for user-facing messages.
pub fn file_store_path() -> Result<PathBuf> {
    file_store::path()
}

/// Enumerate public account identifiers and aliases known to ngit's
/// credential backends. Platform keyrings do not provide a portable listing
/// API, so successful writes are mirrored into a non-secret registry; the
/// file store is scanned as well to cover entries created before that
/// registry existed.
pub fn inventory() -> Result<CredentialInventory> {
    let mut inventory = account_registry::read()?;
    let file_inventory = file_store::inventory()?;
    inventory.accounts.extend(file_inventory.accounts);
    inventory.aliases.extend(file_inventory.aliases);
    Ok(inventory)
}

fn remember_account_for_listing(npub: &str) {
    if let Err(error) = account_registry::remember_account(npub) {
        eprintln!(
            "warning: the signer was stored, but its public identity could not be added to the account-list index: {error:#}"
        );
    }
}

fn remember_alias_for_listing(alias: &str, npub: &str) {
    if let Err(error) = account_registry::remember_alias(alias, npub) {
        eprintln!(
            "warning: the signer alias was stored, but it could not be added to the account-list index: {error:#}"
        );
    }
}

pub fn retrieve(name: &str) -> std::result::Result<Keys, LookupError> {
    let os_error = match retrieve_from(name, Backend::Os) {
        Ok(keys) => return Ok(keys),
        Err(LookupError::Missing(_)) => None,
        Err(LookupError::Unavailable(error)) => Some(error),
        Err(error @ LookupError::Invalid(_)) => return Err(error),
    };
    match retrieve_from(name, Backend::File) {
        Ok(keys) => Ok(keys),
        Err(LookupError::Missing(_)) => os_error.map_or_else(
            || Err(LookupError::Missing(name.to_string())),
            |error| Err(LookupError::Unavailable(error)),
        ),
        Err(LookupError::Unavailable(error)) => Err(LookupError::Unavailable(match os_error {
            Some(os_error) => {
                error.context(format!("OS credential store also failed: {os_error:#}"))
            }
            None => error,
        })),
        Err(error @ LookupError::Invalid(_)) => Err(error),
    }
}

pub fn retrieve_from(name: &str, backend: Backend) -> std::result::Result<Keys, LookupError> {
    let expected = parse_pointer(name).ok_or_else(|| LookupError::Missing(name.to_string()))?;
    match backend {
        Backend::Os if os_store_disabled() => Err(LookupError::Missing(name.to_string())),
        Backend::Os => match os_store::get(name) {
            Ok(keys) if key_matches_npub(&keys, expected) => {
                remember_account_for_listing(expected);
                Ok(keys)
            }
            Ok(_) | Err(OsError::Corrupt) => Err(LookupError::Invalid(format!(
                "OS credential '{name}' does not match its npub"
            ))),
            Err(OsError::NoEntry) => Err(LookupError::Missing(name.to_string())),
            Err(error @ OsError::Store(_)) => Err(LookupError::Unavailable(anyhow!(error))),
        },
        Backend::File => match file_store::get_value(name) {
            Ok(Some(value)) => {
                let keys = Keys::parse(&value).map_err(|_| {
                    LookupError::Invalid(format!(
                        "file credential '{name}' does not contain a nostr secret key"
                    ))
                })?;
                if key_matches_npub(&keys, expected) {
                    Ok(keys)
                } else {
                    Err(LookupError::Invalid(format!(
                        "file credential '{name}' does not match its npub"
                    )))
                }
            }
            Ok(None) => Err(LookupError::Missing(name.to_string())),
            Err(error) => Err(LookupError::Unavailable(error)),
        },
    }
}

fn key_matches_npub(keys: &Keys, expected: &str) -> bool {
    keys.public_key()
        .to_bech32()
        .is_ok_and(|npub| npub == expected)
}

/// Remove the named entry from every backend; `Ok(true)` when an entry was
/// actually deleted from at least one of them.
pub fn forget(name: &str) -> Result<bool> {
    if !valid_entry_name(name) {
        return Ok(false);
    }
    let mut deleted = forget_from_backends(name)?;
    if let Some(alias) = name.strip_prefix(ALIAS_PREFIX) {
        deleted |= forget_from_backends(&alias_credential_entry_name(alias)?)?;
        if let Err(error) = account_registry::forget_alias(alias) {
            eprintln!(
                "warning: the signer alias was removed, but its account-list index entry could not be updated: {error:#}"
            );
        }
    }
    Ok(deleted)
}

fn forget_from_backends(name: &str) -> Result<bool> {
    let mut deleted = false;
    if !os_store_disabled() {
        match os_store::delete(name) {
            Ok(()) => deleted = true,
            Err(OsError::NoEntry) => {}
            Err(error) => {
                return Err(anyhow!(error).context("failed to delete OS credential store entry"));
            }
        }
    }
    if file_store::delete(name)? {
        deleted = true;
    }
    Ok(deleted)
}

pub fn valid_entry_name(name: &str) -> bool {
    parse_pointer(name).is_some()
        || is_bunker_signer_entry_name(name)
        || name
            .strip_prefix(ALIAS_PREFIX)
            .is_some_and(|alias| normalize_alias(alias).is_ok())
        || name
            .strip_prefix(ALIAS_CREDENTIAL_PREFIX)
            .is_some_and(|alias| normalize_alias(alias).is_ok())
}

/// Credential-store entries used by the login in a Git-config scope.
///
/// `resolved_npub` is the identity returned by successful signer
/// construction. Using it avoids resolving an alias a second time with a
/// subtly different precedence during logout or deletion.
pub fn config_pointers(
    git_repo: &Option<&crate::git::Repo>,
    resolved_npub: Option<&str>,
) -> Vec<String> {
    let mut pointers: std::collections::BTreeSet<String> = ["nostr.nsec", "nostr.bunker-app-key"]
        .iter()
        .filter_map(|item| {
            crate::git::get_git_config_item(git_repo, item)
                .ok()
                .flatten()
        })
        .filter(|value| parse_pointer(value).is_some())
        .collect();

    if pointers.is_empty() {
        if let Some(selector) = crate::git::get_git_config_item(git_repo, "nostr.signer")
            .ok()
            .flatten()
        {
            if let Ok(alias) = normalize_alias(&selector) {
                if let Ok(target) = retrieve_signer_alias(&alias) {
                    if let Ok(binding) = alias_credential_entry_name(&alias) {
                        pointers.insert(binding);
                    }
                    if let Some(credential) = target.credential {
                        pointers.insert(credential);
                        return pointers.into_iter().collect();
                    }
                }
            }
            let npub = if let Some(npub) = resolved_npub {
                Some(npub.to_string())
            } else if selector.starts_with("npub1") && PublicKey::parse(&selector).is_ok() {
                Some(selector)
            } else if let Ok(alias) = normalize_alias(&selector) {
                retrieve_alias(&alias).ok().or_else(|| {
                    crate::git::get_git_config_item(
                        git_repo,
                        &format!("nostr.signer-alias.{alias}"),
                    )
                    .ok()
                    .flatten()
                })
            } else {
                None
            };
            if let Some(npub) = npub {
                if !matches!(retrieve(&npub), Err(LookupError::Missing(_))) {
                    pointers.insert(npub.clone());
                }
                if !matches!(retrieve_bunker_signer(&npub), Err(LookupError::Missing(_)))
                    && signer_entry_name(&npub).is_ok()
                {
                    pointers.insert(format!("{SIGNER_PREFIX}{npub}"));
                }
            }
        }
    }
    pointers.into_iter().collect()
}

/// Why an OS credential store operation did not yield a key.
///
/// `NoEntry` is separated from the other cases because the callers must
/// distinguish "nothing stored here" — an ordinary outcome that falls
/// through to the file store — from "the store is broken", which must be
/// reported rather than silently treated as a missing login.
#[derive(Debug)]
enum OsError {
    /// No such entry: never stored, or already deleted.
    NoEntry,
    /// The entry exists but does not hold a nostr secret key.
    Corrupt,
    /// The store itself could not be reached or used.
    Store(KeyringError),
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEntry => write!(f, "no such entry in the OS credential store"),
            Self::Corrupt => write!(
                f,
                "OS credential store entry does not contain a nostr secret key"
            ),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for OsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::NoEntry | Self::Corrupt => None,
        }
    }
}

/// Non-secret index for credential-store identities. Platform keyrings expose
/// entry lookup but no portable enumeration API, so this file records only
/// canonical npubs and alias mappings after successful writes.
mod account_registry {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        io::Write,
        path::{Path, PathBuf},
    };

    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};
    use tempfile::NamedTempFile;

    use super::{CredentialInventory, canonical_npub, normalize_alias};

    #[derive(Default, Serialize, Deserialize)]
    struct Registry {
        accounts: BTreeSet<String>,
        aliases: BTreeMap<String, String>,
    }

    fn path() -> Result<PathBuf> {
        // Keep all test writes beside the debug-only redirected credential
        // file so tests can never touch the real user data directory.
        #[cfg(debug_assertions)]
        if let Ok(credentials) = std::env::var(super::FILE_ENV) {
            let mut indexed = PathBuf::from(credentials).into_os_string();
            indexed.push(".accounts");
            return Ok(PathBuf::from(indexed));
        }
        Ok(crate::get_dirs()?.data_dir().join("accounts.json"))
    }

    pub(super) fn read() -> Result<CredentialInventory> {
        let registry = read_at(&path()?)?;
        let accounts = registry
            .accounts
            .into_iter()
            .map(|npub| canonical_npub(&npub))
            .collect::<Result<BTreeSet<_>>>()?;
        let aliases = registry
            .aliases
            .into_iter()
            .map(|(alias, npub)| Ok((normalize_alias(&alias)?, canonical_npub(&npub)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(CredentialInventory { accounts, aliases })
    }

    pub(super) fn remember_account(npub: &str) -> Result<()> {
        update(|registry| Ok(registry.accounts.insert(canonical_npub(npub)?)))
    }

    pub(super) fn remember_alias(alias: &str, npub: &str) -> Result<()> {
        update(|registry| {
            let alias = normalize_alias(alias)?;
            let npub = canonical_npub(npub)?;
            if registry.aliases.get(&alias) == Some(&npub) {
                Ok(false)
            } else {
                registry.aliases.insert(alias, npub);
                Ok(true)
            }
        })
    }

    pub(super) fn forget_alias(alias: &str) -> Result<()> {
        let alias = normalize_alias(alias)?;
        update(|registry| Ok(registry.aliases.remove(&alias).is_some()))
    }

    fn update(mut change: impl FnMut(&mut Registry) -> Result<bool>) -> Result<()> {
        let path = path()?;
        let mut registry = read_at(&path)?;
        if change(&mut registry)? {
            write_at(&path, &registry)?;
        }
        Ok(())
    }

    fn read_at(path: &Path) -> Result<Registry> {
        if !path.exists() {
            return Ok(Registry::default());
        }
        let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        if data.is_empty() {
            return Ok(Registry::default());
        }
        serde_json::from_slice(&data).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn write_at(path: &Path, registry: &Registry) -> Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let data = serde_json::to_vec_pretty(registry)
            .context("failed to serialize the account registry")?;
        let mut temp = NamedTempFile::new_in(parent).with_context(|| {
            format!("failed to create a temporary file in {}", parent.display())
        })?;
        temp.write_all(&data)
            .and_then(|()| temp.as_file().sync_all())
            .with_context(|| format!("failed to write a replacement for {}", path.display()))?;
        temp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to sync {}", parent.display()))?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn inventory_at(path: &Path) -> Result<CredentialInventory> {
        let registry = read_at(path)?;
        Ok(CredentialInventory {
            accounts: registry.accounts,
            aliases: registry.aliases,
        })
    }

    #[cfg(test)]
    pub(super) fn write_inventory_at(path: &Path, inventory: &CredentialInventory) -> Result<()> {
        write_at(
            path,
            &Registry {
                accounts: inventory.accounts.clone(),
                aliases: inventory.aliases.clone(),
            },
        )
    }
}

/// OS credential store access.
///
/// Inlined from `nostr-keyring`, which upstream discontinued as too thin a
/// wrapper over `keyring` (nostrdevkit/nostr#1414).
///
/// Secrets are written as an `nsec1…` string rather than as raw bytes so
/// that the OS credential manager can display them. The store may hold the
/// only copy of an identity key, so a user must be able to recover it
/// through the platform's own UI — Keychain Access, seahorse, Credential
/// Manager — without ngit present and working.
///
/// Hex-encoded secrets are also read for compatibility with applications
/// that use that textual representation. Other values are rejected as
/// corrupt rather than guessed at.
mod os_store {
    use keyring::Entry;
    use nostr::prelude::{Keys, SecretKey, ToBech32};
    use zeroize::Zeroizing;

    use super::{KeyringError, OsError};

    fn entry(name: &str) -> Result<Entry, OsError> {
        Entry::new(super::SERVICE, name).map_err(OsError::Store)
    }

    pub fn set(name: &str, keys: &Keys) -> Result<(), OsError> {
        let nsec = Zeroizing::new(
            keys.secret_key()
                .to_bech32()
                // nostr declares `Err = Infallible` for this impl.
                .expect("bech32 encoding of a secret key cannot fail"),
        );
        entry(name)?
            .set_password(nsec.as_str())
            .map_err(OsError::Store)
    }

    pub fn set_value(name: &str, value: &str) -> Result<(), OsError> {
        entry(name)?.set_password(value).map_err(OsError::Store)
    }

    pub fn get_value(name: &str) -> Result<String, OsError> {
        match entry(name)?.get_password() {
            Ok(value) => Ok(value),
            Err(KeyringError::BadEncoding(_)) => Err(OsError::Corrupt),
            Err(KeyringError::NoEntry) => Err(OsError::NoEntry),
            Err(error) => Err(OsError::Store(error)),
        }
    }

    pub fn get(name: &str) -> Result<Keys, OsError> {
        let secret_key = match entry(name)?.get_password() {
            Ok(password) => {
                SecretKey::parse(Zeroizing::new(password).as_str()).map_err(|_| OsError::Corrupt)?
            }
            Err(KeyringError::BadEncoding(_)) => return Err(OsError::Corrupt),
            Err(KeyringError::NoEntry) => return Err(OsError::NoEntry),
            Err(error) => return Err(OsError::Store(error)),
        };
        Ok(Keys::new(secret_key))
    }

    pub fn delete(name: &str) -> Result<(), OsError> {
        match entry(name)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(KeyringError::NoEntry) => Err(OsError::NoEntry),
            Err(error) => Err(OsError::Store(error)),
        }
    }
}

/// JSON-file secret store used when no OS credential store is available or
/// the policy selects `file`. Deliberately plaintext — the threat it counters
/// is incidental disclosure of git config, not filesystem compromise (see
/// docs/credential-storage.md) — but access-restricted on unix: 0700
/// directory, 0600 file.
mod file_store {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        io::Write,
        path::{Path, PathBuf},
    };

    use anyhow::{Context, Result};
    use nostr::prelude::Keys;
    use tempfile::NamedTempFile;

    use super::{
        ALIAS_PREFIX, ALIAS_SIGNER_PREFIX, BunkerSignerRecord, CredentialInventory, SERVICE,
        SIGNER_PREFIX, canonical_npub, normalize_alias, parse_pointer, parse_signer_alias,
    };

    pub fn path() -> Result<PathBuf> {
        // Compiled out of release builds so an environment variable can never
        // redirect production secrets.
        #[cfg(debug_assertions)]
        if let Ok(path) = std::env::var(super::FILE_ENV) {
            return Ok(PathBuf::from(path));
        }
        Ok(crate::get_dirs()?.data_dir().join("credentials.json"))
    }

    fn entry_key(name: &str) -> String {
        format!("{}/{name}", super::SERVICE)
    }

    pub fn set(name: &str, keys: &Keys) -> Result<()> {
        set_at(&path()?, name, keys)
    }

    pub fn get(name: &str) -> Result<Option<Keys>> {
        get_at(&path()?, name)
    }

    pub fn delete(name: &str) -> Result<bool> {
        delete_at(&path()?, name)
    }

    pub fn set_value(name: &str, value: &str) -> Result<()> {
        set_value_at(&path()?, name, value)
    }

    pub fn get_value(name: &str) -> Result<Option<String>> {
        get_value_at(&path()?, name)
    }

    pub(super) fn inventory() -> Result<CredentialInventory> {
        inventory_at(&path()?)
    }

    pub(super) fn inventory_at(path: &Path) -> Result<CredentialInventory> {
        let mut accounts = BTreeSet::new();
        let mut aliases = BTreeMap::new();
        let prefix = format!("{SERVICE}/");
        for (key, value) in read(path)? {
            let Some(name) = key.strip_prefix(&prefix) else {
                continue;
            };
            if let Some(npub) = parse_pointer(name) {
                accounts.insert(canonical_npub(npub)?);
            } else if name.strip_prefix(ALIAS_SIGNER_PREFIX).is_some() {
                let record: BunkerSignerRecord = serde_json::from_str(&value)
                    .with_context(|| format!("credential '{name}' is not a valid signer record"))?;
                accounts.insert(canonical_npub(&record.npub)?);
            } else if let Some(npub) = name.strip_prefix(SIGNER_PREFIX) {
                accounts.insert(canonical_npub(npub)?);
            } else if let Some(alias) = name.strip_prefix(ALIAS_PREFIX) {
                aliases.insert(
                    normalize_alias(alias)?,
                    parse_signer_alias(name, &value)
                        .map_err(anyhow::Error::new)?
                        .npub,
                );
            }
        }
        Ok(CredentialInventory { accounts, aliases })
    }

    pub(super) fn set_at(path: &Path, name: &str, keys: &Keys) -> Result<()> {
        let mut values = read(path)?;
        values.insert(entry_key(name), keys.secret_key().to_secret_hex());
        write(path, &values)
    }

    pub(super) fn get_at(path: &Path, name: &str) -> Result<Option<Keys>> {
        Ok(read(path)?
            .get(&entry_key(name))
            .and_then(|hex| Keys::parse(hex).ok()))
    }

    pub(super) fn delete_at(path: &Path, name: &str) -> Result<bool> {
        let mut values = read(path)?;
        if values.remove(&entry_key(name)).is_none() {
            return Ok(false);
        }
        write(path, &values)?;
        Ok(true)
    }

    pub(super) fn set_value_at(path: &Path, name: &str, value: &str) -> Result<()> {
        let mut values = read(path)?;
        values.insert(entry_key(name), value.to_string());
        write(path, &values)
    }

    pub(super) fn get_value_at(path: &Path, name: &str) -> Result<Option<String>> {
        Ok(read(path)?.get(&entry_key(name)).cloned())
    }

    fn read(path: &Path) -> Result<BTreeMap<String, String>> {
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        if data.is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_slice(&data).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn write(path: &Path, values: &BTreeMap<String, String>) -> Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let data = serde_json::to_vec_pretty(values).context("failed to serialize file store")?;
        let mut temp = NamedTempFile::new_in(parent).with_context(|| {
            format!("failed to create a temporary file in {}", parent.display())
        })?;
        temp.write_all(&data)
            .and_then(|()| temp.as_file().sync_all())
            .with_context(|| format!("failed to write a replacement for {}", path.display()))?;
        persist_replacement(temp, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to sync {}", parent.display()))?;
        Ok(())
    }

    #[cfg(not(windows))]
    fn persist_replacement(temp: NamedTempFile, path: &Path) -> std::io::Result<()> {
        temp.persist(path).map(|_| ()).map_err(|error| error.error)
    }

    #[cfg(windows)]
    fn persist_replacement(temp: NamedTempFile, path: &Path) -> std::io::Result<()> {
        use std::{io, iter, os::windows::ffi::OsStrExt, ptr};

        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

        // ReplaceFileW can replace a target while readers that share delete
        // access still hold its previous identity. Close the temporary file
        // before calling it because Windows opens the replacement exclusively.
        let temp_path = temp.into_temp_path();
        let replaced_path = path
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect::<Vec<_>>();
        let replacement_path = temp_path
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect::<Vec<_>>();
        let replaced = unsafe {
            ReplaceFileW(
                replaced_path.as_ptr(),
                replacement_path.as_ptr(),
                ptr::null(),
                0,
                ptr::null(),
                ptr::null(),
            )
        };
        if replaced != 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return temp_path
                .persist(path)
                .map(|_| ())
                .map_err(|error| error.error);
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_round_trip_and_verification() -> Result<()> {
        let keys = Keys::generate();
        let name = entry_name(&keys)?;
        let npub = keys.public_key().to_bech32()?;
        assert_eq!(name, npub, "entry name is the bare npub");
        assert_eq!(parse_pointer(&name), Some(npub.as_str()));
        // legacy pre-release entry names still parse to their npub
        assert_eq!(
            parse_pointer(&format!("{npub}/abcd1234")),
            Some(npub.as_str())
        );
        assert!(parse_pointer(&format!("{npub}/short")).is_none());
        assert!(parse_pointer("npub1notavalidkey").is_none());
        assert!(parse_pointer(&keys.secret_key().to_secret_hex()).is_none());
        Ok(())
    }

    #[test]
    fn classifies_config_values() -> Result<()> {
        let keys = Keys::generate();
        let nsec = keys.secret_key().to_bech32()?;
        let pointer = entry_name(&keys)?;
        assert_eq!(classify_nsec(&nsec), ConfigSecret::Plaintext(&nsec));
        assert!(matches!(
            classify_nsec("ncryptsec1example"),
            ConfigSecret::Encrypted(_)
        ));
        assert_eq!(classify_nsec(&pointer), ConfigSecret::Pointer(&pointer));
        assert_eq!(
            classify_app_key("deadbeef"),
            ConfigSecret::Plaintext("deadbeef")
        );
        Ok(())
    }

    #[test]
    fn npub_prefix_verifies_the_retrieved_key() -> Result<()> {
        let expected = Keys::generate();
        let other = Keys::generate();
        let npub = expected.public_key().to_bech32()?;
        assert!(key_matches_npub(&expected, &npub));
        assert!(!key_matches_npub(&other, &npub));
        Ok(())
    }

    #[test]
    fn policy_values_parse() {
        assert_eq!(parse_policy("auto"), Some(SecretStorage::Auto));
        assert_eq!(parse_policy("FILE"), Some(SecretStorage::File));
        assert_eq!(parse_policy("git-config"), Some(SecretStorage::GitConfig));
        // legacy boolean spellings from the pre-release on/off switch
        assert_eq!(parse_policy("true"), Some(SecretStorage::Auto));
        assert_eq!(parse_policy("false"), Some(SecretStorage::GitConfig));
        assert_eq!(parse_policy("sometimes"), None);
    }

    #[test]
    fn file_store_round_trip_delete_and_permissions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store").join("credentials.json");
        let keys = Keys::generate();
        let name = entry_name(&keys)?;
        file_store::set_at(&path, &name, &keys)?;
        let retrieved = file_store::get_at(&path, &name)?.context("entry missing after set")?;
        assert_eq!(retrieved.public_key(), keys.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)?.permissions().mode() & 0o777,
                0o600,
                "file store must not be group/world readable"
            );
            assert_eq!(
                std::fs::metadata(path.parent().context("no parent")?)?
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "file store directory must not be group/world accessible"
            );
        }
        assert!(file_store::delete_at(&path, &name)?);
        assert!(!file_store::delete_at(&path, &name)?);
        assert!(file_store::get_at(&path, &name)?.is_none());
        Ok(())
    }

    #[test]
    fn typed_bunker_record_is_bundled_and_drops_pairing_secret() -> Result<()> {
        let user = Keys::generate();
        let remote_signer = Keys::generate();
        let client = Keys::generate();
        let user_npub = user.public_key().to_bech32()?;
        let uri = format!(
            "bunker://{}?relay=wss%3A%2F%2Frelay.example.com&secret=one-time",
            remote_signer.public_key()
        );
        let sanitized = sanitized_bunker_uri(&uri)?;
        assert!(!sanitized.contains("secret="));

        let record = BunkerSignerRecord {
            version: SIGNER_RECORD_VERSION,
            record_type: "bunker".to_string(),
            npub: user_npub.clone(),
            bunker_uri: sanitized.clone(),
            client_nsec: client.secret_key().to_bech32()?,
        };
        let name = signer_entry_name(&user_npub)?;
        let parsed = parse_bunker_signer(&name, &user_npub, &serde_json::to_string(&record)?)?;
        assert_eq!(parsed.npub, user_npub);
        assert_eq!(parsed.bunker_uri, sanitized);
        assert_eq!(
            Keys::parse(&parsed.client_nsec)?.public_key(),
            client.public_key()
        );

        let mut mismatched = record;
        mismatched.npub = Keys::generate().public_key().to_bech32()?;
        assert!(
            parse_bunker_signer(&name, &user_npub, &serde_json::to_string(&mismatched)?).is_err()
        );
        Ok(())
    }

    #[test]
    fn file_store_atomically_replaces_the_complete_document() -> Result<()> {
        use std::{collections::BTreeMap, io::Read};

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("credentials.json");
        let first = Keys::generate().public_key().to_bech32()?;
        let second = Keys::generate().public_key().to_bech32()?;
        file_store::set_value_at(&path, "alias:first", &first)?;
        let mut previous_document = std::fs::File::open(&path)?;

        file_store::set_value_at(&path, "alias:second", &second)?;

        let mut previous_data = String::new();
        previous_document.read_to_string(&mut previous_data)?;
        let previous: BTreeMap<String, String> = serde_json::from_str(&previous_data)?;
        assert_eq!(previous.get("nostr/alias:first"), Some(&first));
        assert!(!previous.contains_key("nostr/alias:second"));

        let current: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(current.get("nostr/alias:first"), Some(&first));
        assert_eq!(current.get("nostr/alias:second"), Some(&second));
        Ok(())
    }

    #[test]
    fn file_store_round_trips_typed_signer_and_alias_records() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("credentials.json");
        let user = Keys::generate();
        let remote_signer = Keys::generate();
        let client = Keys::generate();
        let npub = user.public_key().to_bech32()?;
        let signer_name = signer_entry_name(&npub)?;
        let signer_value = serde_json::to_string(&BunkerSignerRecord {
            version: SIGNER_RECORD_VERSION,
            record_type: "bunker".to_string(),
            npub: npub.clone(),
            bunker_uri: format!(
                "bunker://{}?relay=wss%3A%2F%2Frelay.example.com",
                remote_signer.public_key()
            ),
            client_nsec: client.secret_key().to_bech32()?,
        })?;
        file_store::set_value_at(&path, &signer_name, &signer_value)?;
        file_store::set_value_at(&path, "alias:fred", &npub)?;

        assert_eq!(
            file_store::get_value_at(&path, &signer_name)?.as_deref(),
            Some(signer_value.as_str())
        );
        assert_eq!(
            file_store::get_value_at(&path, "alias:fred")?.as_deref(),
            Some(npub.as_str())
        );
        Ok(())
    }

    #[test]
    fn file_store_inventory_recovers_accounts_and_aliases_without_an_index() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("credentials.json");
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        file_store::set_at(&path, &npub, &keys)?;
        file_store::set_value_at(&path, "alias:dcdev", &npub)?;
        let signer_name = alias_signer_entry_name("remote")?;
        let signer_value = serde_json::to_string(&BunkerSignerRecord {
            version: SIGNER_RECORD_VERSION,
            record_type: "bunker".to_string(),
            npub: npub.clone(),
            bunker_uri: format!(
                "bunker://{}?relay=wss%3A%2F%2Frelay.example.com",
                Keys::generate().public_key()
            ),
            client_nsec: Keys::generate().secret_key().to_bech32()?,
        })?;
        file_store::set_value_at(&path, &signer_name, &signer_value)?;
        file_store::set_value_at(&path, "alias:remote", &npub)?;

        let inventory = file_store::inventory_at(&path)?;
        assert_eq!(inventory.accounts, BTreeSet::from([npub.clone()]));
        assert_eq!(
            inventory.aliases,
            BTreeMap::from([
                ("dcdev".to_string(), npub.clone()),
                ("remote".to_string(), npub),
            ])
        );
        Ok(())
    }

    #[test]
    fn account_registry_contains_only_public_identity_data() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("accounts.json");
        let npub = Keys::generate().public_key().to_bech32()?;
        let inventory = CredentialInventory {
            accounts: BTreeSet::from([npub.clone()]),
            aliases: BTreeMap::from([("dcdev".to_string(), npub.clone())]),
        };

        account_registry::write_inventory_at(&path, &inventory)?;
        assert_eq!(account_registry::inventory_at(&path)?, inventory);
        let contents = std::fs::read_to_string(path)?;
        assert!(contents.contains(&npub));
        assert!(contents.contains("dcdev"));
        assert!(!contents.contains("nsec1"));
        Ok(())
    }

    #[test]
    fn alias_names_are_portable_and_namespaced() -> Result<()> {
        assert_eq!(normalize_alias("Fred-2")?, "fred-2");
        assert_eq!(alias_entry_name("Fred-2")?, "alias:fred-2");
        assert_eq!(
            alias_credential_entry_name("Fred-2")?,
            "alias-credential:fred-2"
        );
        assert_eq!(alias_signer_entry_name("Fred-2")?, "signer-alias:fred-2");
        for invalid in [
            "",
            "-fred",
            "2fred",
            "fred_jones",
            "fred.jones",
            "fred jones",
        ] {
            assert!(
                normalize_alias(invalid).is_err(),
                "accepted invalid alias {invalid:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn intermediate_typed_alias_remains_readable_for_migration() -> Result<()> {
        let npub = Keys::generate().public_key().to_bech32()?;
        let credential = alias_signer_entry_name("dedicated")?;
        let value = serde_json::to_string(&SignerAliasRecord {
            version: ALIAS_RECORD_VERSION,
            record_type: "alias".to_string(),
            npub: npub.clone(),
            credential: credential.clone(),
        })?;

        assert_eq!(
            parse_signer_alias("alias:dedicated", &value)?,
            SignerAlias {
                npub: npub.clone(),
                credential: Some(credential),
            }
        );
        assert_eq!(
            parse_signer_alias("alias:legacy", &npub)?,
            SignerAlias {
                npub,
                credential: None,
            }
        );
        assert!(parse_signer_alias("alias:other", &value).is_err());
        Ok(())
    }

    #[test]
    fn signer_credential_conflicts_remain_distinguishable_from_backend_failures() {
        let error = anyhow::Error::new(SignerCredentialConflict {
            npub: "npub1example".to_string(),
        });

        assert!(is_signer_credential_conflict(&error));
        assert!(!is_signer_credential_conflict(&anyhow!(
            "credential backend unavailable"
        )));
    }

    #[test]
    fn file_values_cannot_be_hidden_by_different_os_data() {
        assert!(ensure_unshadowed_file_value("signer:npub", "new", None).is_ok());
        assert!(ensure_unshadowed_file_value("signer:npub", "same", Some("same")).is_ok());
        assert!(ensure_unshadowed_file_value("signer:npub", "new", Some("old")).is_err());
    }
}
