//! Account creation and missing-account guidance tests.
//!
//! Drives `ngit account create --local --name "..." --relay <url>` against one
//! vanilla nostr relay that is *not* injected into `NGIT_RELAY_DEFAULT_SET` —
//! the relay is only reachable because the test passes it explicitly via
//! `--relay`. Asserts:
//!
//! 1. The command exits successfully.
//! 2. The generated nsec / npub land in the repo's *local* git config —
//!    `--local` means we should not write to global config.
//! 3. The relay received the user's kind 0 metadata event with the requested
//!    display name.
//! 4. The relay received the user's kind 10002 relay-list event listing that
//!    same relay as a write/read target.
//!
//! No exact-stdout assertions, no `#[serial]`, no PTY — entire flow
//! exercises only `Command`, the harness env-var injection, and the
//! relay's real wire query.

use anyhow::{Context, Result};
use ngit::login::credential_store::SERVICE;
use nostr_sdk::prelude::*;
use serde_json::Value;
use tempfile::NamedTempFile;
use test_harness::{Harness, repo::Repo};

#[tokio::test]
async fn export_keys_without_account_suggests_login_or_creation() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;

    let output = repo
        .ngit(["account", "export-keys"])
        .output()
        .await
        .context("failed to spawn ngit account export-keys")?;

    assert!(
        !output.status.success(),
        "expected account export-keys to fail without an account"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nostr account required"),
        "expected account-required error, got: {stderr}"
    );
    assert!(
        stderr.contains("ngit account login") && stderr.contains("ngit account create"),
        "expected login and account creation guidance, got: {stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn account_create_relay_arg_publishes_metadata_and_relay_list() -> Result<()> {
    // Register under "target" — not "default" — so NGIT_RELAY_DEFAULT_SET is
    // unset. The relay is only reachable via the explicit --relay argument.
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("target")
    .build()
    .await?;

    let repo = harness.fresh_repo()?;
    let relay_url = harness.relay("target").url().to_string();

    let display_name = "lighthouse alice";

    let output = repo
        .ngit([
            "account",
            "create",
            "--local",
            "--name",
            display_name,
            "--relay",
            &relay_url,
        ])
        .output()
        .await
        .context("failed to spawn ngit account create")?;

    assert!(
        output.status.success(),
        "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // --- assertion 2: credentials saved to local git config ---------------

    let nsec = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after `account create --local`")?;
    let npub = repo
        .config("nostr.npub")
        .await?
        .context("nostr.npub missing from local git config after `account create --local`")?;

    let keys = Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")?;
    assert_eq!(
        npub,
        keys.public_key().to_bech32()?,
        "stored npub does not match nsec"
    );

    // --- assertion 3: kind 0 metadata reached the specified relay ----------

    let pubkey = keys.public_key();
    let metadata_events = harness
        .relay("target")
        .events(Filter::new().author(pubkey).kind(Kind::Metadata))
        .await?;

    assert_eq!(
        metadata_events.len(),
        1,
        "expected exactly one kind 0 event from the new account, got {}: {:?}",
        metadata_events.len(),
        metadata_events,
    );
    let metadata = Metadata::from_json(&metadata_events[0].content)
        .context("kind 0 event content is not valid Metadata JSON")?;
    assert_eq!(
        metadata.name.as_deref(),
        Some(display_name),
        "metadata.name does not match --name argument",
    );

    // --- assertion 4: kind 10002 relay-list reached the specified relay ----

    let relay_list_events = harness
        .relay("target")
        .events(Filter::new().author(pubkey).kind(Kind::RelayList))
        .await?;

    assert_eq!(
        relay_list_events.len(),
        1,
        "expected exactly one kind 10002 event from the new account, got {}",
        relay_list_events.len(),
    );

    let listed_relays: Vec<String> = relay_list_events[0]
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("r") {
                s.get(1).cloned()
            } else {
                None
            }
        })
        .collect();

    // Relay URLs can be normalised by nostr-sdk (trailing slash, ws vs wss
    // canonicalisation). Compare loosely on the host:port substring.
    let host_port = relay_url.trim_start_matches("ws://").trim_end_matches('/');
    assert!(
        listed_relays.iter().any(|r| r.contains(host_port)),
        "relay list does not include the specified relay {relay_url:?}; \
         got entries: {listed_relays:?}",
    );

    Ok(())
}

#[tokio::test]
async fn credential_file_stores_pointer_and_logout_keeps_entry_until_forgotten() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;

    let output = repo
        .ngit(["account", "create", "--local", "--name", "keyring alice"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "account create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let pointer = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec pointer missing")?;
    assert_eq!(
        Some(pointer.as_str()),
        repo.config("nostr.npub").await?.as_deref(),
        "entry name should be the account npub"
    );
    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert!(
        entries.get(format!("{SERVICE}/{pointer}")).is_some(),
        "credential file lacks pointer entry"
    );

    let output = repo
        .ngit(["account", "export-keys"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "pointer-backed export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = repo
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.config("nostr.nsec").await?.is_none());
    // Logout keeps the stored secret - the store may hold the only copy of
    // the key - and points at the explicit removal command instead.
    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert!(
        entries.get(format!("{SERVICE}/{pointer}")).is_some(),
        "logout must retain the credential entry"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("forget-keys"),
        "logout should name the removal command: {stderr}"
    );

    let output = repo
        .ngit(["account", "forget-keys", &pointer])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "forget-keys failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert!(
        entries.get(format!("{SERVICE}/{pointer}")).is_none(),
        "forget-keys must remove the credential entry"
    );
    Ok(())
}

#[tokio::test]
async fn logout_forget_removes_entry() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let nsec = Keys::generate().secret_key().to_bech32()?;
    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pointer = repo
        .config("nostr.nsec")
        .await?
        .context("pointer missing after login")?;

    let output = repo
        .ngit(["account", "logout", "--forget"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "logout --forget failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.config("nostr.nsec").await?.is_none());
    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert!(
        entries.get(format!("{SERVICE}/{pointer}")).is_none(),
        "logout --forget must remove the credential entry"
    );
    Ok(())
}

#[tokio::test]
async fn login_alias_selects_stored_nsec_without_rewriting_profile() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--alias",
            "fred",
            "--nsec",
            &nsec,
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "aliased login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.config("nostr.signer").await?.as_deref(), Some("fred"));
    assert!(
        repo.config("nostr.nsec").await?.is_none(),
        "a credential-backed alias should be the sole local signer selector"
    );
    assert_eq!(
        repo.config("nostr.signer-alias.fred").await?.as_deref(),
        Some(npub.as_str())
    );
    assert_eq!(
        repo.config("nostr.npub").await?.as_deref(),
        Some(npub.as_str())
    );

    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert_eq!(
        entries
            .get(format!("{SERVICE}/alias:fred"))
            .and_then(Value::as_str),
        Some(npub.as_str())
    );
    assert!(entries.get(format!("{SERVICE}/{npub}")).is_some());

    let unset = repo
        .git(["config", "--local", "--unset", "nostr.signer"])
        .output()
        .await?;
    assert!(unset.status.success());
    let output = repo
        .ngit(["--signer", "fred", "account", "export-keys"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "alias selection failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.config("nostr.signer").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn alias_only_login_reactivates_a_retained_file_signer() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &nsec,
            "--alias",
            "dcagent",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "initial login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = repo
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.config("nostr.nsec").await?.is_none());

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--alias",
            "dcagent",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "alias-only login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some("dcagent")
    );
    assert_eq!(
        repo.config("nostr.signer-alias.dcagent").await?.as_deref(),
        Some(npub.as_str())
    );
    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert_eq!(
        entries
            .get(format!("{SERVICE}/alias:dcagent"))
            .and_then(Value::as_str),
        Some(npub.as_str())
    );
    assert!(entries.get(format!("{SERVICE}/{npub}")).is_some());
    Ok(())
}

#[tokio::test]
async fn npub_only_login_reactivates_a_retained_file_signer() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;

    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(output.status.success());

    let output = repo
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(output.status.success());

    let output = repo
        .ngit([
            "--signer",
            &npub,
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "npub-only login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some(npub.as_str())
    );
    assert!(
        repo.config("nostr.nsec").await?.is_none(),
        "a credential-backed npub should be the sole local signer selector"
    );
    Ok(())
}

#[tokio::test]
async fn selected_git_config_signer_survives_account_switching_logout() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--secret-storage",
            "git-config",
            "--nsec",
            &nsec,
            "--alias",
            "fred",
        ])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "initial plaintext login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = repo
        .ngit([
            "--signer",
            "fred",
            "account",
            "login",
            "--local",
            "--offline",
            "--secret-storage",
            "git-config",
        ])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "Git-config signer was lost during account switching: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.nsec").await?.as_deref(),
        Some(nsec.as_str())
    );
    assert_eq!(repo.config("nostr.signer").await?.as_deref(), Some("fred"));
    assert_eq!(
        repo.config("nostr.npub").await?.as_deref(),
        Some(npub.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn unknown_alias_does_not_log_out_the_current_account() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;

    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(output.status.success());

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--alias",
            "unknown",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        !output.status.success(),
        "unknown alias unexpectedly logged in"
    );
    assert_eq!(
        repo.config("nostr.nsec").await?.as_deref(),
        Some(npub.as_str()),
        "failed selection must preserve the current login"
    );
    Ok(())
}

#[tokio::test]
async fn credential_store_alias_cannot_be_reassigned_to_another_account() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let original = Keys::generate();
    let original_nsec = original.secret_key().to_bech32()?;
    let original_npub = original.public_key().to_bech32()?;
    let replacement_nsec = Keys::generate().secret_key().to_bech32()?;

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &original_nsec,
            "--alias",
            "fred",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(output.status.success());

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &replacement_nsec,
            "--alias",
            "fred",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(!output.status.success(), "alias reassignment was accepted");
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some("fred"),
        "rejected alias reassignment must preserve the selected alias"
    );
    assert_eq!(
        repo.config("nostr.npub").await?.as_deref(),
        Some(original_npub.as_str()),
        "rejected alias reassignment must preserve the selected identity"
    );
    assert!(
        repo.config("nostr.nsec").await?.is_none(),
        "credential-backed aliases must not restore a redundant nsec pointer"
    );
    let entries: Value = serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    assert_eq!(
        entries
            .get(format!("{SERVICE}/alias:fred"))
            .and_then(Value::as_str),
        Some(original_npub.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn git_config_only_alias_can_name_different_accounts_in_two_repos() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let first = harness.fresh_repo()?;
    let second = harness.fresh_repo()?;
    let first_keys = Keys::generate();
    let second_keys = Keys::generate();

    for (repo, keys) in [(&first, &first_keys), (&second, &second_keys)] {
        let nsec = keys.secret_key().to_bech32()?;
        let output = repo
            .ngit([
                "account",
                "login",
                "--local",
                "--offline",
                "--secret-storage",
                "git-config",
                "--nsec",
                &nsec,
                "--alias",
                "fred",
            ])
            .output()
            .await?;
        assert!(
            output.status.success(),
            "Git-only alias login failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first_npub = first_keys.public_key().to_bech32()?;
    let second_npub = second_keys.public_key().to_bech32()?;
    assert_eq!(
        first.config("nostr.signer-alias.fred").await?.as_deref(),
        Some(first_npub.as_str())
    );
    assert_eq!(
        second.config("nostr.signer-alias.fred").await?.as_deref(),
        Some(second_npub.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn local_alias_can_reactivate_a_global_git_config_signer() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let config_home = tempfile::tempdir()?;
    let git_config_dir = config_home.path().join("git");
    std::fs::create_dir(&git_config_dir)?;
    let global = git_config_dir.join("config");
    let credentials = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;
    let global_path = global
        .as_path()
        .to_str()
        .context("global config path is not UTF-8")?;
    for (key, value) in [
        ("nostr.nsec", nsec.as_str()),
        ("nostr.npub", npub.as_str()),
        ("nostr.signer-alias.fred", npub.as_str()),
    ] {
        let output = repo
            .git(["config", "--file", global_path, key, value])
            .output()
            .await?;
        assert!(output.status.success());
    }

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--secret-storage",
            "git-config",
            "--alias",
            "fred",
        ])
        .env_remove("NGITTEST")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env("XDG_CONFIG_HOME", config_home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "local alias login from global material failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.config("nostr.signer").await?.as_deref(), Some("fred"));
    assert_eq!(
        repo.config("nostr.nsec").await?.as_deref(),
        Some(nsec.as_str())
    );
    let global_nsec = repo
        .git(["config", "--file", global_path, "--get", "nostr.nsec"])
        .output()
        .await?;
    assert!(global_nsec.status.success(), "global signer was removed");
    Ok(())
}

#[tokio::test]
async fn multiple_file_signers_remain_independently_selectable() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let first = harness.fresh_repo()?;
    let second = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let fred = Keys::generate();
    let alice = Keys::generate();

    for (repo, alias, keys) in [(&first, "fred", &fred), (&second, "alice", &alice)] {
        let nsec = keys.secret_key().to_bech32()?;
        let output = repo
            .ngit([
                "account",
                "login",
                "--local",
                "--offline",
                "--nsec",
                &nsec,
                "--alias",
                alias,
            ])
            .env("NGIT_SECRET_STORAGE", "file")
            .env("NGIT_KEYRING_FILE", credentials.path())
            .output()
            .await?;
        assert!(output.status.success());
    }

    let output = first
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--alias",
            "alice",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "second stored signer was not selectable in another repo: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let alice_npub = alice.public_key().to_bech32()?;
    assert_eq!(
        first.config("nostr.npub").await?.as_deref(),
        Some(alice_npub.as_str())
    );
    let entries: Value = serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    for (alias, keys) in [("fred", &fred), ("alice", &alice)] {
        let npub = keys.public_key().to_bech32()?;
        assert!(entries.get(format!("{SERVICE}/{npub}")).is_some());
        assert_eq!(
            entries
                .get(format!("{SERVICE}/alias:{alias}"))
                .and_then(Value::as_str),
            Some(npub.as_str())
        );
    }
    Ok(())
}

#[tokio::test]
async fn invalid_new_signer_inputs_do_not_log_out_the_current_account() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;
    let app_key = Keys::generate().secret_key().to_secret_hex();

    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(output.status.success());

    for invalid_args in [
        vec!["--nsec", "not-a-secret"],
        vec!["--nsec", "ncryptsec1not-valid"],
        vec!["--bunker-url", "not-a-bunker-url"],
        vec![
            "--bunker-uri",
            "not-a-bunker-uri",
            "--bunker-app-key",
            &app_key,
        ],
    ] {
        let output = repo
            .ngit(
                ["account", "login", "--local", "--offline"]
                    .into_iter()
                    .chain(invalid_args),
            )
            .env("NGIT_SECRET_STORAGE", "file")
            .env("NGIT_KEYRING_FILE", credentials.path())
            .output()
            .await?;
        assert!(
            !output.status.success(),
            "invalid signer input was accepted"
        );
        assert_eq!(
            repo.config("nostr.nsec").await?.as_deref(),
            Some(npub.as_str()),
            "invalid signer input must preserve the current login"
        );
    }
    Ok(())
}

#[tokio::test]
async fn direct_ncryptsec_login_uses_the_cli_password() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let keys = Keys::generate();
    let password = "correct horse battery staple";
    let encrypted = nostr::nips::nip49::EncryptedSecretKey::new(
        keys.secret_key(),
        password,
        1,
        nostr::nips::nip49::KeySecurity::Medium,
    )?
    .to_bech32()?;

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &encrypted,
            "--password",
            password,
        ])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "encrypted login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.nsec").await?.as_deref(),
        Some(encrypted.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn local_login_outside_a_repo_does_not_store_the_secret() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let outside = tempfile::tempdir()?;
    let credentials = NamedTempFile::new()?;
    let nsec = Keys::generate().secret_key().to_bech32()?;
    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .current_dir(outside.path())
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;

    assert!(
        !output.status.success(),
        "local login unexpectedly succeeded"
    );
    assert!(
        std::fs::read(credentials.path())?.is_empty(),
        "a rejected local login must not write a credential"
    );
    Ok(())
}

#[tokio::test]
async fn credentials_json_alias_mapping_precedes_git_config() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let file_alias_keys = Keys::generate();
    let git_alias_keys = Keys::generate();
    let file_alias_npub = file_alias_keys.public_key().to_bech32()?;
    let git_alias_npub = git_alias_keys.public_key().to_bech32()?;
    let mut entries = serde_json::Map::new();
    entries.insert(
        format!("{SERVICE}/alias:fred"),
        Value::String(file_alias_npub.clone()),
    );
    for (npub, keys) in [
        (&file_alias_npub, &file_alias_keys),
        (&git_alias_npub, &git_alias_keys),
    ] {
        entries.insert(
            format!("{SERVICE}/{npub}"),
            Value::String(keys.secret_key().to_secret_hex()),
        );
    }
    std::fs::write(file.path(), serde_json::to_vec(&entries)?)?;
    repo.git_ok(
        [
            "config",
            "--local",
            "nostr.signer-alias.fred",
            &git_alias_npub,
        ],
        "seed lower-priority git alias",
    )
    .await?;

    let output = repo
        .ngit([
            "--signer",
            "fred",
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "selected login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.npub").await?.as_deref(),
        Some(file_alias_npub.as_str()),
        "credentials.json must override the git config alias mapping"
    );
    Ok(())
}

#[tokio::test]
async fn logout_forget_uses_the_identity_selected_before_git_alias_mapping() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let selected_keys = Keys::generate();
    let git_alias_keys = Keys::generate();
    let selected_npub = selected_keys.public_key().to_bech32()?;
    let git_alias_npub = git_alias_keys.public_key().to_bech32()?;
    let mut entries = serde_json::Map::new();
    entries.insert(
        format!("{SERVICE}/alias:fred"),
        Value::String(selected_npub.clone()),
    );
    for (npub, keys) in [
        (&selected_npub, &selected_keys),
        (&git_alias_npub, &git_alias_keys),
    ] {
        entries.insert(
            format!("{SERVICE}/{npub}"),
            Value::String(keys.secret_key().to_secret_hex()),
        );
    }
    std::fs::write(file.path(), serde_json::to_vec(&entries)?)?;
    repo.git_ok(
        [
            "config",
            "--local",
            "nostr.signer-alias.fred",
            &git_alias_npub,
        ],
        "seed lower-priority git alias",
    )
    .await?;
    repo.git_ok(
        ["config", "--local", "nostr.signer", "fred"],
        "select the conflicting alias",
    )
    .await?;

    let output = repo
        .ngit(["account", "logout", "--forget"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "logout --forget failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let entries: Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
    assert!(
        entries.get(format!("{SERVICE}/{selected_npub}")).is_none(),
        "logout must forget the identity selected from credentials.json"
    );
    assert!(
        entries.get(format!("{SERVICE}/{git_alias_npub}")).is_some(),
        "logout must not forget the conflicting lower-priority Git alias identity"
    );
    Ok(())
}

#[tokio::test]
async fn logout_clears_broken_local_selection_before_a_valid_global_login() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let global = NamedTempFile::new()?;
    let global_keys = Keys::generate();
    let global_nsec = global_keys.secret_key().to_bech32()?;
    repo.git_ok(
        ["config", "--local", "nostr.signer", "missing"],
        "seed broken local signer selection",
    )
    .await?;
    let global_path = global
        .path()
        .to_str()
        .context("global config path is not UTF-8")?;
    let output = repo
        .git(["config", "--file", global_path, "nostr.nsec", &global_nsec])
        .output()
        .await?;
    assert!(output.status.success());

    let output = repo
        .ngit(["account", "logout"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", global.path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.config("nostr.signer").await?.is_none());

    let global_nsec_after = repo
        .git(["config", "--file", global_path, "--get", "nostr.nsec"])
        .output()
        .await?;
    assert!(global_nsec_after.status.success());
    assert_eq!(
        String::from_utf8(global_nsec_after.stdout)?.trim(),
        global_nsec,
        "logout must leave the lower-priority global login intact"
    );
    Ok(())
}

#[tokio::test]
async fn plaintext_is_read_without_migration_and_dangling_pointer_has_login_guidance() -> Result<()>
{
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    repo.git_ok(
        ["config", "--local", "nostr.nsec", &nsec],
        "seed plaintext nsec",
    )
    .await?;

    // Reading a plaintext login with the credential store enabled leaves it
    // untouched: moving it into a store only happens via `ngit account
    // login`.
    let output = repo
        .ngit(["account", "export-keys"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "export-keys with plaintext nsec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.nsec").await?.as_deref(),
        Some(nsec.as_str()),
        "plaintext nsec must not be rewritten by a read"
    );
    assert!(
        std::fs::read(file.path())?.is_empty(),
        "no credential entry may be written by a read"
    );

    // Log in with the store enabled to obtain a pointer, then wipe the
    // store to make it dangle.
    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "login into credential store failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pointer = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec pointer missing after login")?;
    assert_ne!(pointer, nsec);
    let expected_npub = keys.public_key().to_bech32()?;
    assert_eq!(
        repo.config("nostr.npub").await?.as_deref(),
        Some(expected_npub.as_str())
    );

    std::fs::write(file.path(), b"{}")?;
    let output = repo
        .ngit(["account", "export-keys"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("credential") && stderr.contains("ngit account login"),
        "missing dangling-pointer guidance: {stderr}"
    );
    let output = repo
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(output.status.success());
    assert!(repo.config("nostr.nsec").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn local_logins_for_same_key_share_one_entry() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let first = harness.fresh_repo()?;
    let second = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let nsec = Keys::generate().secret_key().to_bech32()?;
    for repo in [&first, &second] {
        let output = repo
            .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
            .env("NGIT_SECRET_STORAGE", "auto")
            .env("NGIT_KEYRING_FILE", file.path())
            .output()
            .await?;
        assert!(
            output.status.success(),
            "login failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let first_pointer = first
        .config("nostr.nsec")
        .await?
        .context("first pointer missing")?;
    let second_pointer = second
        .config("nostr.nsec")
        .await?
        .context("second pointer missing")?;
    // one entry per account: both logins point at the same npub-named entry,
    // which is safe because logout keeps entries instead of deleting them
    assert_eq!(first_pointer, second_pointer);

    let output = first
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(output.status.success());
    let output = second
        .ngit(["account", "export-keys"])
        .env("NGIT_SECRET_STORAGE", "auto")
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "second login was broken by first logout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// `account create --local --name <name>` stores the fresh secret in the
/// file store and saves the account's kind-0 profile into the repo-scoped
/// global cache; the follow-up logout retains both. Returns the npub.
async fn create_named_account_then_logout(
    repo: &Repo,
    credentials: &NamedTempFile,
    name: &str,
) -> Result<String> {
    let output = repo
        .ngit(["account", "create", "--local", "--name", name])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "account create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let npub = repo
        .config("nostr.npub")
        .await?
        .context("nostr.npub missing after account create")?;
    let output = repo
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(npub)
}

#[tokio::test]
async fn profile_name_selects_the_sole_credentialed_account_for_one_command() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    create_named_account_then_logout(&repo, &credentials, "Lighthouse Alice").await?;

    // case-insensitive match against the cached kind-0 profile
    let output = repo
        .ngit(["--signer", "lighthouse ALICE", "account", "export-keys"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "profile-name selection failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // a one-shot selection resolves per invocation and persists nothing
    assert!(repo.config("nostr.signer").await?.is_none());
    assert!(repo.config("nostr.npub").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn login_with_profile_name_persists_the_npub_not_the_name() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let npub =
        create_named_account_then_logout(&repo, &credentials, "Casper The Friendly Ghost").await?;

    let output = repo
        .ngit([
            "--signer",
            "casper the friendly ghost",
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "profile-name login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some(npub.as_str()),
        "login must persist the resolved npub, never the profile name"
    );
    assert_eq!(
        repo.config("nostr.npub").await?.as_deref(),
        Some(npub.as_str())
    );
    assert!(
        repo.config("nostr.nsec").await?.is_none(),
        "a credential-backed selection must not restore a redundant nsec pointer"
    );
    Ok(())
}

#[tokio::test]
async fn same_named_cached_profile_without_credentials_is_ignored() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;

    // squat the name: the profile stays cached but its credentials are gone
    let squatter_npub =
        create_named_account_then_logout(&repo, &credentials, "Shared Name").await?;
    let output = repo
        .ngit(["account", "forget-keys", &squatter_npub])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "forget-keys failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let real_npub = create_named_account_then_logout(&repo, &credentials, "shared name").await?;

    let output = repo
        .ngit([
            "--signer",
            "Shared Name",
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "the sole credentialed account was not selected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some(real_npub.as_str()),
        "selection must resolve to the credentialed account"
    );
    Ok(())
}

#[tokio::test]
async fn two_credentialed_same_named_accounts_fail_closed() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let first = create_named_account_then_logout(&repo, &credentials, "Shared Name").await?;
    let second = create_named_account_then_logout(&repo, &credentials, "Shared Name").await?;

    let output = repo
        .ngit([
            "--signer",
            "shared name",
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        !output.status.success(),
        "ambiguous profile name was accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&first) && stderr.contains(&second),
        "ambiguity error must list each candidate npub: {stderr}"
    );
    for key in ["nostr.signer", "nostr.npub", "nostr.nsec"] {
        assert!(
            repo.config(key).await?.is_none(),
            "failed selection must not change {key}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unknown_profile_name_yields_guidance_and_preserves_the_login() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;
    let output = repo
        .ngit(["account", "login", "--local", "--offline", "--nsec", &nsec])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(output.status.success());

    // both a name-shaped selector and a valid-but-unmapped alias token get
    // cache guidance rather than a bare alias-not-found error
    for selector in ["No Such Name", "nosuchname"] {
        let output = repo
            .ngit([
                "--signer",
                selector,
                "account",
                "login",
                "--local",
                "--offline",
            ])
            .env("NGIT_SECRET_STORAGE", "file")
            .env("NGIT_KEYRING_FILE", credentials.path())
            .output()
            .await?;
        assert!(
            !output.status.success(),
            "unknown profile name '{selector}' unexpectedly logged in"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--signer <npub>"),
            "missing selector guidance for '{selector}': {stderr}"
        );
        assert!(
            stderr.to_lowercase().contains("cache"),
            "missing cache explanation for '{selector}': {stderr}"
        );
        assert_eq!(
            repo.config("nostr.nsec").await?.as_deref(),
            Some(npub.as_str()),
            "failed selection must preserve the current login"
        );
    }
    Ok(())
}
