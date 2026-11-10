//! Independent account operations must not lose each other's JSON updates.
use std::time::Duration;

use anyhow::{Context, Result};
use nostr::prelude::{Keys, ToBech32};
use serde_json::Value;
use test_harness::Harness;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_profile_imports_and_deletions_preserve_other_accounts() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = repo.dir().join("credentials.json");
    let registry = repo.dir().join("credentials.json.accounts");
    let original_config = std::fs::read(repo.dir().join(".git/config"))?;
    let identities = (0..24).map(|_| Keys::generate()).collect::<Vec<_>>();
    let mut children = Vec::new();
    for (index, keys) in identities.iter().enumerate() {
        children.push(
            repo.ngit([
                "account",
                "login",
                "--nsec",
                &keys.secret_key().to_bech32()?,
                "--profile",
                &format!("profile-{index}"),
                "--no-activate",
                "--secret-storage",
                "file",
                "--offline",
                "--json",
            ])
            .env("NGIT_KEYRING_FILE", &credentials)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?,
        );
    }
    for child in children {
        let output =
            tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await??;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stored: Value = serde_json::from_slice(&std::fs::read(&credentials)?)?;
    let indexed: Value = serde_json::from_slice(&std::fs::read(&registry)?)?;
    assert_eq!(
        indexed["accounts"]
            .as_array()
            .context("missing account index")?
            .len(),
        identities.len()
    );
    for (index, keys) in identities.iter().enumerate() {
        let alias = format!("profile-{index}");
        let npub = keys.public_key().to_bech32()?;
        let secret = stored[format!("nostr/{npub}")]
            .as_str()
            .context("successful import lost its secret")?;
        assert!(
            Keys::parse(secret)?.public_key() == keys.public_key(),
            "stored key changed"
        );
        assert_eq!(stored[format!("nostr/alias:{alias}")], npub);
        assert_eq!(indexed["aliases"][&alias], npub);
    }
    assert_eq!(
        std::fs::read(repo.dir().join(".git/config"))?,
        original_config
    );

    let mut children = Vec::new();
    for keys in &identities {
        children.push(
            repo.ngit(["account", "forget-keys", &keys.public_key().to_bech32()?])
                .env("NGIT_KEYRING_FILE", &credentials)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()?,
        );
    }
    for child in children {
        let output =
            tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await??;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let remaining: Value = serde_json::from_slice(&std::fs::read(&credentials)?)?;
    assert!(
        remaining
            .as_object()
            .context("missing credential map")?
            .keys()
            .all(|entry| entry.starts_with("nostr/forgotten-profile:"))
    );
    let indexed: Value = serde_json::from_slice(&std::fs::read(&registry)?)?;
    assert_eq!(indexed["accounts"], serde_json::json!([]));
    assert_eq!(indexed["aliases"], serde_json::json!({}));
    Ok(())
}
