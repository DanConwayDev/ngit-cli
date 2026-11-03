use std::{path::Path, process::Output, time::Duration};

use anyhow::{Context, Result};
use nostr::prelude::{Keys, ToBech32};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
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

async fn command(repo: &Repo, file: &Path, args: &[&str]) -> Result<Output> {
    let mut command = repo.ngit(args);
    command
        .env("NGIT_KEYRING_FILE", file)
        .env("NGIT_SECRET_STORAGE", "file")
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .context("account operation waited for an unavailable signer")?
        .context("account command failed to launch")
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn contents(file: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(file)?)?)
}

#[tokio::test]
async fn forget_after_logout_removes_only_selected_profile_without_contacting_signer() -> Result<()>
{
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let other = session(&npub)?;
    let initial = json!({
        "nostr/signer-alias:work": session(&npub)?,
        "nostr/alias:work": npub,
        "nostr/alias-credential:work": "signer-alias:work",
        "nostr/signer-alias:host": other,
        "nostr/alias:host": npub,
        "nostr/alias-credential:host": "signer-alias:host",
    });
    std::fs::write(file.path(), serde_json::to_vec(&initial)?)?;
    repo.git_ok(["config", "nostr.signer", "work"], "select work")
        .await?;
    success(
        &command(
            &repo,
            file.path(),
            &["account", "logout", "--local", "--keep-credentials"],
        )
        .await?,
    );
    assert_eq!(contents(file.path())?, initial);
    assert_eq!(repo.config("nostr.signer").await?, None);
    // The old record can even be corrupt: public profile metadata suffices.
    let mut broken = initial;
    broken["nostr/signer-alias:work"] = json!("invalid session data");
    std::fs::write(file.path(), serde_json::to_vec(&broken)?)?;
    repo.git_ok(
        ["config", "nostr.signer", "host"],
        "select unrelated profile",
    )
    .await?;
    success(
        &command(
            &repo,
            file.path(),
            &["--signer", "work", "account", "logout", "--forget"],
        )
        .await?,
    );
    let stored = contents(file.path())?;
    assert!(stored.get("nostr/signer-alias:work").is_none());
    assert!(stored.get("nostr/alias:work").is_none());
    assert!(stored.get("nostr/alias-credential:work").is_none());
    assert_eq!(stored["nostr/signer-alias:host"], other);
    // A stale portable Git alias must not fall back to the remaining session.
    repo.git_ok(
        ["config", "nostr.signer-alias.work", &npub],
        "seed stale portable mapping",
    )
    .await?;
    let stale = command(
        &repo,
        file.path(),
        &["--signer", "work", "account", "export-keys", "--json"],
    )
    .await?;
    assert!(!stale.status.success());

    assert_eq!(repo.config("nostr.signer").await?.as_deref(), Some("host"));
    // Explicit fresh credentials reactivate the name and clear its guard.
    success(
        &command(
            &repo,
            file.path(),
            &[
                "account",
                "login",
                "--local",
                "--offline",
                "--profile",
                "work",
                "--secret-storage",
                "git-config",
                "--nsec",
                &keys.secret_key().to_bech32()?,
            ],
        )
        .await?,
    );
    assert!(
        contents(file.path())?
            .get("nostr/forgotten-profile:work")
            .is_none()
    );
    success(
        &command(
            &repo,
            file.path(),
            &["--signer", "work", "account", "export-keys", "--json"],
        )
        .await?,
    );
    Ok(())
}

#[tokio::test]
async fn global_logout_discards_remote_session_unless_retention_requested() -> Result<()> {
    let harness = harness().await?;
    for keep in [false, true] {
        let repo = harness.fresh_repo()?;
        let file = NamedTempFile::new()?;
        let npub = Keys::generate().public_key().to_bech32()?;
        let entry = format!("nostr/signer:{npub}");
        std::fs::write(
            file.path(),
            serde_json::to_vec(&json!({entry.clone(): session(&npub)?}))?,
        )?;
        let global_path = repo.dir().join("global-config");
        git2::Config::open(&global_path)?.set_str("nostr.signer", &npub)?;
        let mut args = vec!["account", "logout", "--global"];
        if keep {
            args.push("--keep-credentials");
        }
        let output = repo
            .ngit(args)
            .env_remove("NGITTEST")
            .env("GIT_CONFIG_GLOBAL", &global_path)
            .env("NGIT_KEYRING_FILE", file.path())
            .output()
            .await?;
        success(&output);
        assert_eq!(contents(file.path())?.get(&entry).is_some(), keep);
        assert!(
            git2::Config::open(&global_path)?
                .get_string("nostr.signer")
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn forgetting_last_profile_cleans_inventory_and_ambiguous_npub_is_rejected() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let npub = Keys::generate().public_key().to_bech32()?;
    let initial = json!({
        "nostr/signer-alias:one": session(&npub)?, "nostr/alias:one": npub,
        "nostr/alias-credential:one": "signer-alias:one",
        "nostr/signer-alias:two": session(&npub)?, "nostr/alias:two": npub,
        "nostr/alias-credential:two": "signer-alias:two",
    });
    std::fs::write(file.path(), serde_json::to_vec(&initial)?)?;
    let mut registry = file.path().as_os_str().to_os_string();
    registry.push(".accounts");
    std::fs::write(
        Path::new(&registry),
        serde_json::to_vec(&json!({
            "accounts": [npub], "aliases": {"one": npub, "two": npub},
        }))?,
    )?;
    let output = command(
        &repo,
        file.path(),
        &["account", "logout", &npub, "--forget"],
    )
    .await?;
    assert!(!output.status.success());
    assert_eq!(contents(file.path())?, initial);
    for name in ["one", "two"] {
        success(
            &command(
                &repo,
                file.path(),
                &["account", "logout", "--profile", name, "--forget"],
            )
            .await?,
        );
    }
    assert_eq!(
        contents(file.path())?,
        json!({
            "nostr/forgotten-profile:one": "removed",
            "nostr/forgotten-profile:two": "removed",
        })
    );
    let inventory = contents(Path::new(&registry))?;
    assert_eq!(inventory["accounts"], json!([]));
    assert_eq!(inventory["aliases"], json!({}));
    let whoami = command(
        &repo,
        file.path(),
        &["account", "whoami", "--offline", "--json"],
    )
    .await?;
    success(&whoami);
    let document: Value = serde_json::from_slice(&whoami.stdout)?;
    assert_eq!(document["accounts"], json!([]));
    let output = command(&repo, file.path(), &["account", "logout", "--forget"]).await?;
    assert!(
        !output.status.success(),
        "forget without a selection must explain the missing target"
    );
    Ok(())
}

#[tokio::test]
async fn global_logout_requires_consent_for_identity_keys_and_removes_shared_sessions() -> Result<()>
{
    let harness = harness().await?;
    for remote in [false, true] {
        let repo = harness.fresh_repo()?;
        let file = NamedTempFile::new()?;
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        let entry = if remote {
            format!("nostr/signer:{npub}")
        } else {
            format!("nostr/{npub}")
        };
        let secret = if remote {
            session(&npub)?
        } else {
            keys.secret_key().to_bech32()?
        };
        let initial = json!({entry: secret});
        std::fs::write(file.path(), serde_json::to_vec(&initial)?)?;
        let global_path = repo.dir().join("global-config");
        git2::Config::open(&global_path)?.set_str("nostr.signer", &npub)?;
        if remote {
            repo.git_ok(
                ["config", "nostr.signer", &npub],
                "share the global session locally",
            )
            .await?;
        }
        let output = repo
            .ngit(["account", "logout", "--global"])
            .env_remove("NGITTEST")
            .env("GIT_CONFIG_GLOBAL", &global_path)
            .env("NGIT_KEYRING_FILE", file.path())
            .output()
            .await?;
        if remote {
            success(&output);
            assert_eq!(contents(file.path())?, json!({}));
        } else {
            assert!(!output.status.success());
            assert_eq!(contents(file.path())?, initial);
            assert_eq!(
                git2::Config::open(&global_path)?.get_string("nostr.signer")?,
                npub
            );
        }
        if remote {
            assert_eq!(
                repo.config("nostr.signer").await?.as_deref(),
                Some(npub.as_str())
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn global_logout_discards_a_legacy_remote_app_key_pointer() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let client = Keys::generate();
    let pointer = client.public_key().to_bech32()?;
    std::fs::write(
        file.path(),
        serde_json::to_vec(&json!({
            format!("nostr/{pointer}"): client.secret_key().to_bech32()?,
        }))?,
    )?;
    let global_path = repo.dir().join("global-config");
    let mut global = git2::Config::open(&global_path)?;
    global.set_str(
        "nostr.bunker-uri",
        &format!("bunker://{}", Keys::generate().public_key()),
    )?;
    global.set_str("nostr.bunker-app-key", &pointer)?;
    let output = repo
        .ngit(["account", "logout", "--global"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_path)
        .env("NGIT_KEYRING_FILE", file.path())
        .output()
        .await?;
    success(&output);
    assert_eq!(contents(file.path())?, json!({}));
    assert!(global.get_string("nostr.bunker-app-key").is_err());
    Ok(())
}

#[tokio::test]
async fn forget_follows_the_selector_instead_of_a_stale_legacy_pointer() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let file = NamedTempFile::new()?;
    let selected = Keys::generate().public_key().to_bech32()?;
    let other = Keys::generate();
    let other_npub = other.public_key().to_bech32()?;
    std::fs::write(
        file.path(),
        serde_json::to_vec(&json!({
            "nostr/signer-alias:work": session(&selected)?,
            "nostr/alias:work": selected,
            "nostr/alias-credential:work": "signer-alias:work",
            format!("nostr/{other_npub}"): other.secret_key().to_bech32()?,
        }))?,
    )?;
    repo.git_ok(["config", "nostr.signer", "work"], "select work")
        .await?;
    repo.git_ok(["config", "nostr.nsec", &other_npub], "seed stale pointer")
        .await?;
    success(&command(&repo, file.path(), &["account", "logout", "--forget"]).await?);
    let stored = contents(file.path())?;
    assert!(stored.get("nostr/signer-alias:work").is_none());
    assert!(stored.get(format!("nostr/{other_npub}")).is_some());
    Ok(())
}
