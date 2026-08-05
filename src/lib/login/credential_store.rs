use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use keyring::Error as KeyringError;
use nostr::{Keys, PublicKey, ToBech32};
use nostr_keyring::NostrKeyring;

pub const SERVICE: &str = "ngit";
/// Debug-build-only override selecting the plaintext file store for tests.
pub const FILE_ENV: &str = "NGIT_KEYRING_FILE";
pub const ENABLE_ENV: &str = "NGIT_CREDENTIAL_STORE";

pub fn enabled(git_repo: &Option<&crate::git::Repo>) -> bool {
    if let Some(value) = std::env::var(ENABLE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_bool)
    {
        return value;
    }
    if git_repo.is_some() {
        if let Some(value) = crate::git::get_git_config_item(git_repo, "nostr.credential-store")
            .ok()
            .flatten()
            .and_then(|value| parse_bool(&value))
        {
            return value;
        }
    }
    crate::git::get_git_config_item(&None, "nostr.credential-store")
        .ok()
        .flatten()
        .and_then(|value| parse_bool(&value))
        .unwrap_or(true)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
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
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(
                f,
                "credential '{name}' was not found in the OS credential store"
            ),
            Self::Unavailable(error) => write!(
                f,
                "OS credential store is unavailable: {error}. Restore access to the platform store, or use --nsec / --bunker-uri with --bunker-app-key for one command"
            ),
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

pub fn parse_pointer(value: &str) -> Option<(&str, &str)> {
    let (npub, suffix) = value.split_once('/')?;
    if !npub.starts_with("npub1")
        || suffix.len() != 8
        || !suffix.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    PublicKey::parse(npub).ok()?;
    Some((npub, suffix))
}

pub fn entry_name(keys: &Keys) -> Result<String> {
    // 8 hex chars (32 bits) of throwaway-key randomness: unique enough for a
    // user's handful of logins without adding a rand dependency.
    let random = Keys::generate().secret_key().to_secret_hex();
    Ok(format!(
        "{}/{}",
        keys.public_key().to_bech32()?,
        &random[..8]
    ))
}

pub fn store(keys: &Keys) -> Result<String> {
    configure_test_store();
    let name = entry_name(keys)?;
    let store = NostrKeyring::new(SERVICE);
    store
        .set(&name, keys)
        .context("failed to write OS credential store")?;
    match retrieve(&name) {
        Ok(retrieved) if retrieved.public_key() == keys.public_key() => Ok(name),
        Ok(_) => {
            let _ = store.delete(&name);
            bail!("credential store read-back returned a different key")
        }
        Err(error) => {
            let _ = store.delete(&name);
            Err(anyhow!(error).context("failed to verify OS credential store write"))
        }
    }
}

pub fn retrieve(name: &str) -> std::result::Result<Keys, LookupError> {
    configure_test_store();
    let expected = parse_pointer(name)
        .map(|(npub, _)| npub)
        .ok_or_else(|| LookupError::Missing(name.to_string()))?;
    match NostrKeyring::new(SERVICE).get(name) {
        Ok(keys) if key_matches_npub(&keys, expected) => Ok(keys),
        Ok(_) => Err(LookupError::Missing(name.to_string())),
        Err(error) if is_no_entry(&error) => Err(LookupError::Missing(name.to_string())),
        Err(error) => Err(LookupError::Unavailable(anyhow!(error))),
    }
}

fn key_matches_npub(keys: &Keys, expected: &str) -> bool {
    keys.public_key()
        .to_bech32()
        .is_ok_and(|npub| npub == expected)
}

pub fn delete(name: &str) -> Result<()> {
    if parse_pointer(name).is_none() {
        return Ok(());
    }
    configure_test_store();
    match NostrKeyring::new(SERVICE).delete(name) {
        Ok(()) => Ok(()),
        Err(error) if is_no_entry(&error) => Ok(()),
        Err(error) => Err(anyhow!(error).context("failed to delete OS credential store entry")),
    }
}

/// Delete the keyring entries referenced by any pointer values in the given
/// config scope's `nostr.nsec` / `nostr.bunker-app-key` items.
pub fn delete_config_pointers(git_repo: &Option<&crate::git::Repo>) -> Result<()> {
    for item in ["nostr.nsec", "nostr.bunker-app-key"] {
        if let Some(value) = crate::git::get_git_config_item(git_repo, item)? {
            if parse_pointer(&value).is_some() {
                delete(&value).with_context(|| {
                    format!(
                        "failed to remove keyring entry {value}; remove it via your OS keychain UI"
                    )
                })?;
            }
        }
    }
    Ok(())
}

fn is_no_entry(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(err) = current {
        if matches!(
            err.downcast_ref::<KeyringError>(),
            Some(KeyringError::NoEntry)
        ) {
            return true;
        }
        current = err.source();
    }
    false
}

fn configure_test_store() {
    // Compiled out of release builds so an environment variable can never
    // redirect production secrets away from the platform store.
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var(FILE_ENV) {
        keyring::set_default_credential_builder(Box::new(file_store::FileBuilder(
            std::path::PathBuf::from(path),
        )));
    }
}

/// Plaintext JSON-file credential backend used by integration tests via
/// `NGIT_KEYRING_FILE`. Debug builds only.
#[cfg(debug_assertions)]
mod file_store {
    use std::{any::Any, collections::BTreeMap, fs, path::PathBuf};

    use keyring::{
        Error as KeyringError,
        credential::{Credential, CredentialApi, CredentialBuilderApi, CredentialPersistence},
    };

    #[derive(Debug)]
    pub struct FileBuilder(pub PathBuf);

    impl CredentialBuilderApi for FileBuilder {
        fn build(
            &self,
            _: Option<&str>,
            service: &str,
            user: &str,
        ) -> keyring::Result<Box<Credential>> {
            Ok(Box::new(FileCredential {
                path: self.0.clone(),
                key: format!("{service}/{user}"),
            }))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::UntilDelete
        }
    }

    #[derive(Debug)]
    struct FileCredential {
        path: PathBuf,
        key: String,
    }

    impl FileCredential {
        fn read(&self) -> keyring::Result<BTreeMap<String, String>> {
            if !self.path.exists() {
                return Ok(BTreeMap::new());
            }
            let data = fs::read(&self.path).map_err(platform_error)?;
            if data.is_empty() {
                return Ok(BTreeMap::new());
            }
            serde_json::from_slice(&data).map_err(platform_error)
        }
        fn write(&self, values: &BTreeMap<String, String>) -> keyring::Result<()> {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent).map_err(platform_error)?;
            }
            let data = serde_json::to_vec_pretty(values).map_err(platform_error)?;
            fs::write(&self.path, data).map_err(platform_error)
        }
    }

    impl CredentialApi for FileCredential {
        fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
            let mut values = self.read()?;
            values.insert(self.key.clone(), encode_hex(secret));
            self.write(&values)
        }
        fn get_secret(&self) -> keyring::Result<Vec<u8>> {
            let value = self
                .read()?
                .remove(&self.key)
                .ok_or(KeyringError::NoEntry)?;
            decode_hex(&value)
        }
        fn delete_credential(&self) -> keyring::Result<()> {
            let mut values = self.read()?;
            if values.remove(&self.key).is_none() {
                return Err(KeyringError::NoEntry);
            }
            self.write(&values)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn platform_error(error: impl std::error::Error + Send + Sync + 'static) -> KeyringError {
        KeyringError::PlatformFailure(Box::new(error))
    }

    fn encode_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn decode_hex(value: &str) -> keyring::Result<Vec<u8>> {
        if !value.len().is_multiple_of(2) {
            return Err(KeyringError::Invalid(
                "credential".to_string(),
                "odd-length hex".to_string(),
            ));
        }
        (0..value.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&value[i..i + 2], 16).map_err(|error| {
                    KeyringError::Invalid("credential".to_string(), error.to_string())
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_round_trip_and_verification() -> Result<()> {
        let keys = Keys::generate();
        let name = entry_name(&keys)?;
        let (npub, suffix) = parse_pointer(&name).context("generated pointer did not parse")?;
        assert_eq!(npub, keys.public_key().to_bech32()?);
        assert_eq!(suffix.len(), 8);
        assert!(parse_pointer(&format!("{npub}/short")).is_none());
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
}
