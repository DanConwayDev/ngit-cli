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
async fn plain_global_logout_forgets_named_profiles() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = repo.dir().join("credentials.json");
    let global = repo.dir().join("global-config");
    let npub = Keys::generate().public_key().to_bech32()?;
    let entries = json!({
        "nostr/alias:work": npub, "nostr/alias-credential:work": "signer-alias:work",
        "nostr/signer-alias:work": session(&npub)?,
    });
    std::fs::write(&file, serde_json::to_vec(&entries)?)?;
    git2::Config::open(&global)?.set_str("nostr.signer", "work")?;
    let output = run(&repo, &file, &global, &["account", "logout", "--json"]).await?;
    success(&output);
    assert_eq!(
        stored(&file)?,
        json!({"nostr/forgotten-profile:work": "removed"})
    );
    assert!(
        git2::Config::open(&global)?
            .get_string("nostr.signer")
            .is_err()
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["accounts"], json!([]));
    Ok(())
}

#[tokio::test]
async fn repository_logout_forgets_shared_profile_and_activates_default_account() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let other = harness.fresh_repo()?;
    let file = repo.dir().join("credentials.json");
    let global = repo.dir().join("global-config");
    let work = Keys::generate().public_key().to_bech32()?;
    let personal = Keys::generate().public_key().to_bech32()?;
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            "nostr/alias:work": work,
            "nostr/alias-credential:work": "signer-alias:work",
            "nostr/signer-alias:work": session(&work)?,
            format!("nostr/signer:{personal}"): session(&personal)?,
        }))?,
    )?;
    git2::Config::open(&global)?.set_str("nostr.signer", &personal)?;
    for repository in [&repo, &other] {
        repository
            .git_ok(
                ["config", "nostr.signer", "work"],
                "select shared work profile",
            )
            .await?;
    }
    let output = run(&repo, &file, &global, &["account", "logout", "--json"]).await?;
    success(&output);
    assert_eq!(repo.config("nostr.signer").await?, None);
    assert!(stored(&file)?.get("nostr/signer-alias:work").is_none());
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(document["accounts"][0]["npub"], personal);
    assert_eq!(document["accounts"][0]["active"], true);
    assert_eq!(other.config("nostr.signer").await?.as_deref(), Some("work"));
    let output = run(
        &other,
        &file,
        &global,
        &["account", "whoami", "--offline", "--json"],
    )
    .await?;
    success(&output);
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["unavailable_logins"][0]["scope"], "local");
    assert_eq!(document["accounts"][0]["active"], false);
    // Explicit local logout does not subsequently remove the default login.
    success(&run(&repo, &file, &global, &["account", "logout", "--local"]).await?);
    assert_eq!(
        git2::Config::open(&global)?.get_string("nostr.signer")?,
        personal
    );
    Ok(())
}

#[tokio::test]
async fn plain_logout_requires_consent_before_removing_plaintext_identity_key() -> Result<()> {
    let harness = harness().await?;
    for keep in [false, true] {
        let repo = harness.fresh_repo()?;
        let file = repo.dir().join("credentials.json");
        let global = repo.dir().join("global-config");
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        git2::Config::open(&global)?.set_str("nostr.nsec", &keys.secret_key().to_bech32()?)?;
        let before = std::fs::read(&global)?;
        let output = run(&repo, &file, &global, &["account", "logout"]).await?;
        assert!(!output.status.success());
        assert_eq!(std::fs::read(&global)?, before);
        let option = if keep {
            "--keep-credentials"
        } else {
            "--forget"
        };
        success(&run(&repo, &file, &global, &["account", "logout", option]).await?);
        assert!(
            git2::Config::open(&global)?
                .get_string("nostr.nsec")
                .is_err()
        );
        if keep {
            let data = stored(&file)?;
            assert_eq!(
                Keys::parse(
                    data[format!("nostr/{npub}")]
                        .as_str()
                        .context("missing preserved key")?
                )?
                .public_key(),
                keys.public_key()
            );
        }
    }
    Ok(())
}
