use std::{path::Path, process::Output, time::Duration};

use anyhow::{Context, Result};
use nostr::prelude::{Keys, ToBech32};
use serde_json::{Value, json};
use test_harness::{Harness, repo::Repo};

async fn harness() -> Result<Harness> {
    Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await
}

fn session(npub: &str) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "version": 1, "type": "bunker", "npub": npub,
        "bunker_uri": format!("bunker://{}?relay=ws://127.0.0.1:9", Keys::generate().public_key()),
        "client_nsec": Keys::generate().secret_key().to_bech32()?,
    }))?)
}

async fn run(repo: &Repo, file: &Path, global: &Path, args: &[&str]) -> Result<Output> {
    let mut command = repo.ngit(args);
    command
        .env_remove("NGITTEST")
        .env("NGIT_KEYRING_FILE", file)
        .env("GIT_CONFIG_GLOBAL", global)
        .env("NGIT_SECRET_STORAGE", "file")
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .context("logout waited for a remote signer")?
        .context("failed to launch logout")
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stored(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[tokio::test]
async fn all_removes_profiles_and_nonlocal_includes_but_does_not_search_repositories() -> Result<()>
{
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let other_repo = harness.fresh_repo()?;
    let file = repo.dir().join("credentials.json");
    let global = repo.dir().join("global-config");
    let included = repo.dir().join("included-config");
    let system = repo.dir().join("system-config");
    let npub = Keys::generate().public_key().to_bech32()?;
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            "nostr/alias:host": npub, "nostr/alias-credential:host": "signer-alias:host",
            "nostr/signer-alias:host": session(&npub)?,
            "nostr/alias:vm": npub, "nostr/alias-credential:vm": "signer-alias:vm",
            "nostr/signer-alias:vm": session(&npub)?,
            format!("nostr/signer:{npub}"): session(&npub)?,
            "another-service/untouched": "not an ngit credential",
        }))?,
    )?;
    let mut config = git2::Config::open(&global)?;
    config.set_str("nostr.signer", "host")?;
    config.set_str("includeIf.gitdir:/does/not/match/.path", "included-config")?;
    let mut config = git2::Config::open(&included)?;
    config.set_str("nostr.signer", "vm")?;
    config.set_str("nostr.signer-alias.vm", &npub)?;
    config.set_str("include.path", "global-config")?; // cycle is bounded
    git2::Config::open(&system)?.set_str("nostr.signer", "host")?;
    repo.git_ok(["config", "nostr.signer", "vm"], "select local profile")
        .await?;
    other_repo
        .git_ok(
            ["config", "nostr.signer", "host"],
            "select other repo profile",
        )
        .await?;
    let other_before = std::fs::read(other_repo.dir().join(".git/config"))?;
    let output = repo
        .ngit(["account", "logout", "--all", "--json"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_SYSTEM", &system)
        .env("GIT_CONFIG_NOSYSTEM", "false")
        .env("NGIT_KEYRING_FILE", &file)
        .output()
        .await?;
    success(&output);
    let entries = stored(&file)?;
    assert_eq!(
        entries["another-service/untouched"],
        "not an ngit credential"
    );
    assert!(
        entries
            .as_object()
            .unwrap()
            .keys()
            .all(|key| key.starts_with("nostr/forgotten-profile:")
                || key.starts_with("another-service/"))
    );
    for path in [&global, &included, &system] {
        assert!(
            git2::Config::open(path)?
                .get_string("nostr.signer")
                .is_err()
        );
    }
    assert!(
        git2::Config::open(&included)?
            .get_string("nostr.signer-alias.vm")
            .is_err()
    );
    assert_eq!(repo.config("nostr.signer").await?, None);
    assert_eq!(
        std::fs::read(other_repo.dir().join(".git/config"))?,
        other_before
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["accounts"], json!([]));
    Ok(())
}

#[tokio::test]
async fn all_preflights_identity_keys_before_any_mutation() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = repo.dir().join("credentials.json");
    let global = repo.dir().join("global-config");
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            format!("nostr/{npub}"): keys.secret_key().to_bech32()?,
            format!("nostr/signer:{npub}"): session(&npub)?,
        }))?,
    )?;
    git2::Config::open(&global)?.set_str("nostr.nsec", &keys.secret_key().to_bech32()?)?;
    let credentials_before = std::fs::read(&file)?;
    let config_before = std::fs::read(&global)?;
    let output = run(&repo, &file, &global, &["account", "logout", "--all"]).await?;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&file)?, credentials_before);
    assert_eq!(std::fs::read(&global)?, config_before);
    let output = run(
        &repo,
        &file,
        &global,
        &["account", "logout", "--all", "--forget"],
    )
    .await?;
    success(&output);
    assert_eq!(stored(&file)?, json!({}));
    assert!(
        git2::Config::open(&global)?
            .get_string("nostr.nsec")
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn keep_credentials_preserves_plaintext_identity_keys_and_lists_saved_profiles() -> Result<()>
{
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = repo.dir().join("credentials.json");
    let global = repo.dir().join("global-config");
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    git2::Config::open(&global)?.set_str("nostr.nsec", &keys.secret_key().to_bech32()?)?;
    let output = run(
        &repo,
        &file,
        &global,
        &["account", "logout", "--all", "--keep-credentials", "--json"],
    )
    .await?;
    success(&output);
    let data = stored(&file)?;
    let retained = Keys::parse(
        data[format!("nostr/{npub}")]
            .as_str()
            .context("stored identity key missing")?,
    )?;
    assert_eq!(retained.public_key(), keys.public_key());
    assert!(
        git2::Config::open(&global)?
            .get_string("nostr.nsec")
            .is_err()
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["accounts"][0]["npub"], npub);
    assert_eq!(document["accounts"][0]["active"], false);
    Ok(())
}
