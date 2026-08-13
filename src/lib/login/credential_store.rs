use std::{fmt, path::PathBuf, sync::OnceLock};

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
const ALIAS_PREFIX: &str = "alias:";
const SIGNER_PREFIX: &str = "signer:";

/// A complete credential-store record for a NIP-46 signer. The client key is
/// an application key used to communicate with the bunker, never the user's
/// identity key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BunkerSigner {
    pub npub: String,
    pub bunker_uri: String,
    pub client_nsec: String,
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
        if let Some(policy) = crate::git::get_git_config_item(&None, key)
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

pub fn alias_entry_name(alias: &str) -> Result<String> {
    Ok(format!("{ALIAS_PREFIX}{}", normalize_alias(alias)?))
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
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        bail!("signer alias must be 1 to 64 letters, numbers, or hyphens");
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
            Ok(()) => return Ok((name, Backend::Os)),
            Err(error) => Some(anyhow!(error)),
        }
    };
    store_file(&name, keys).with_context(|| match &os_error {
        Some(error) => format!(
            "failed to write ngit's file store after the OS credential store failed: {error:#}"
        ),
        None => "failed to write ngit's file store".to_string(),
    })?;
    Ok((name, Backend::File))
}

/// Store a complete NIP-46 signer record under `signer:<user-npub>`.
/// Any one-time pairing secret in the supplied bunker URI is deliberately
/// removed before serialization.
pub fn store_bunker_signer(
    npub: &str,
    bunker_uri: &str,
    client_keys: &Keys,
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
    let name = signer_entry_name(&npub)?;
    store_value(&name, serialized.as_str(), policy)
}

pub fn retrieve_bunker_signer(npub: &str) -> std::result::Result<BunkerSigner, LookupError> {
    let expected_npub = canonical_npub(npub)
        .map_err(|error| LookupError::Invalid(format!("invalid selected signer: {error:#}")))?;
    let name = signer_entry_name(&expected_npub)
        .map_err(|error| LookupError::Invalid(error.to_string()))?;
    let serialized = retrieve_value(&name)?;
    parse_bunker_signer(&name, &expected_npub, &serialized)
}

pub fn store_alias(alias: &str, npub: &str, policy: SecretStorage) -> Result<(String, Backend)> {
    let name = alias_entry_name(alias)?;
    let npub = canonical_npub(npub)?;
    store_value(&name, &npub, policy)
}

pub fn retrieve_alias(alias: &str) -> std::result::Result<String, LookupError> {
    let name = alias_entry_name(alias)
        .map_err(|error| LookupError::Invalid(format!("invalid signer alias: {error:#}")))?;
    let npub = retrieve_value(&name)?;
    canonical_npub(&npub).map_err(|error| {
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

fn store_value(name: &str, value: &str, policy: SecretStorage) -> Result<(String, Backend)> {
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

fn retrieve_value(name: &str) -> std::result::Result<String, LookupError> {
    let os_error = if os_store_disabled() {
        None
    } else {
        match os_store::get_value(name) {
            Ok(value) => return Ok(value),
            Err(OsError::NoEntry) => None,
            Err(error) => Some(anyhow!(error)),
        }
    };
    match file_store::get_value(name) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => match os_error {
            Some(error) => Err(LookupError::Unavailable(error)),
            None => Err(LookupError::Missing(name.to_string())),
        },
        Err(error) => Err(LookupError::Unavailable(match os_error {
            Some(os_error) => {
                error.context(format!("OS credential store also failed: {os_error:#}"))
            }
            None => error,
        })),
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

pub fn retrieve(name: &str) -> std::result::Result<Keys, LookupError> {
    let expected = parse_pointer(name).ok_or_else(|| LookupError::Missing(name.to_string()))?;
    let os_error = if os_store_disabled() {
        None
    } else {
        match os_store::get(name) {
            Ok(keys) if key_matches_npub(&keys, expected) => return Ok(keys),
            // an entry that fails npub verification is treated as absent
            Ok(_) => None,
            Err(OsError::NoEntry) => None,
            Err(error) => Some(anyhow!(error)),
        }
    };
    match file_store::get(name) {
        Ok(Some(keys)) if key_matches_npub(&keys, expected) => Ok(keys),
        Ok(_) => match os_error {
            Some(error) => Err(LookupError::Unavailable(error)),
            None => Err(LookupError::Missing(name.to_string())),
        },
        Err(error) => Err(LookupError::Unavailable(match os_error {
            Some(os_error) => {
                error.context(format!("OS credential store also failed: {os_error:#}"))
            }
            None => error,
        })),
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
        || name
            .strip_prefix(SIGNER_PREFIX)
            .is_some_and(|npub| canonical_npub(npub).is_ok())
        || name
            .strip_prefix(ALIAS_PREFIX)
            .is_some_and(|alias| normalize_alias(alias).is_ok())
}

/// Credential-store pointer values in the given config scope's `nostr.nsec`
/// / `nostr.bunker-app-key` items.
pub fn config_pointers(git_repo: &Option<&crate::git::Repo>) -> Vec<String> {
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
            let npub = if selector.starts_with("npub1") && PublicKey::parse(&selector).is_ok() {
                Some(selector)
            } else if let Ok(alias) = normalize_alias(&selector) {
                crate::git::get_git_config_item(git_repo, &format!("nostr.signer-alias.{alias}"))
                    .ok()
                    .flatten()
                    .or_else(|| retrieve_alias(&alias).ok())
            } else {
                None
            };
            if let Some(npub) = npub {
                if signer_entry_name(&npub).is_ok() {
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
        collections::BTreeMap,
        fs,
        io::Write,
        path::{Path, PathBuf},
    };

    use anyhow::{Context, Result};
    use nostr::prelude::Keys;

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
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
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
        }
        let data = serde_json::to_vec_pretty(values).context("failed to serialize file store")?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0600 applies on creation only; a pre-existing file keeps its
            // mode (the tempfile-backed test override relies on this).
            options.mode(0o600);
        }
        options
            .open(path)
            .and_then(|mut file| file.write_all(&data))
            .with_context(|| format!("failed to write {}", path.display()))
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
    fn typed_bunker_record_is_atomic_and_drops_pairing_secret() -> Result<()> {
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
    fn alias_names_are_portable_and_namespaced() -> Result<()> {
        assert_eq!(normalize_alias("Fred-2")?, "fred-2");
        assert_eq!(alias_entry_name("Fred-2")?, "alias:fred-2");
        for invalid in ["", "-fred", "fred_jones", "fred.jones", "fred jones"] {
            assert!(
                normalize_alias(invalid).is_err(),
                "accepted invalid alias {invalid:?}"
            );
        }
        Ok(())
    }
}
