//! Saving profiles for explicit use must not change either login scope.

use anyhow::{Context, Result};
use nostr::prelude::{Keys, ToBech32};
use tempfile::NamedTempFile;
use test_harness::Harness;

#[tokio::test]
async fn imported_profile_does_not_activate_inside_or_outside_repository() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    repo.git_ok(
        ["config", "nostr.signer", "local-default"],
        "seed local login",
    )
    .await?;
    let global = NamedTempFile::new()?;
    std::fs::write(global.path(), "[nostr]\n\tsigner = global-default\n")?;
    let local_path = repo.dir().join(".git/config");
    let local_before = std::fs::read(&local_path)?;
    let global_before = std::fs::read(global.path())?;
    let credentials = NamedTempFile::new()?;
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;

    for (alias, directory) in [("inside", repo.dir()), ("outside", harness.home())] {
        let output = repo
            .ngit([
                "account",
                "login",
                "--profile",
                alias,
                "--no-activate",
                "--nsec",
                &nsec,
                "--offline",
                "--secret-storage",
                "auto",
            ])
            .current_dir(directory)
            .env_remove("NGITTEST")
            .env("GIT_CONFIG_GLOBAL", global.path())
            .env("NGIT_KEYRING_FILE", credentials.path())
            .env("NGIT_TEST_OS_STORE_UNAVAILABLE", "true")
            .output()
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&local_path)?, local_before);
        assert_eq!(std::fs::read(global.path())?, global_before);
        let exported = repo
            .ngit(["--signer", alias, "account", "export-keys", "--json"])
            .env("NGIT_KEYRING_FILE", credentials.path())
            .output()
            .await?;
        assert!(
            exported.status.success(),
            "{}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let document: serde_json::Value = serde_json::from_slice(&exported.stdout)?;
        assert_eq!(document["npub"], npub);
        assert_eq!(
            document["nsec"].as_str().context("missing stored key")?,
            nsec
        );
    }

    // A Git-config storage policy cannot satisfy a store-only request.
    let before = std::fs::read(credentials.path())?;
    let output = repo
        .ngit([
            "account",
            "login",
            "--profile",
            "refused",
            "--no-activate",
            "--nsec",
            &nsec,
            "--offline",
            "--secret-storage",
            "git-config",
        ])
        .env("GIT_CONFIG_GLOBAL", global.path())
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(credentials.path())?, before);
    assert_eq!(std::fs::read(&local_path)?, local_before);
    assert_eq!(std::fs::read(global.path())?, global_before);
    Ok(())
}

#[tokio::test]
async fn ignored_pairing_relay_is_rejected_before_changing_login_or_credentials() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let key = Keys::generate();
    let nsec = key.secret_key().to_bech32()?;
    let output = repo
        .ngit([
            "account",
            "login",
            "--nsec",
            &nsec,
            "--profile",
            "existing",
            "--offline",
        ])
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_path = repo.dir().join(".git/config");
    let config_before = std::fs::read(&config_path)?;
    let credentials_before = std::fs::read(credentials.path())?;
    for selector in [
        vec!["--profile", "existing"],
        vec!["existing"],
        vec!["--signer", "existing"],
        vec!["--nsec", &nsec],
        vec!["--bunker-url", "bunker://invalid"],
    ] {
        let mut args = vec![
            "account",
            "login",
            "--signer-relay",
            "ws://127.0.0.1:1",
            "--offline",
            "--json",
        ];
        args.extend(selector);
        let output = repo
            .ngit(args)
            .env("NGIT_KEYRING_FILE", credentials.path())
            .output()
            .await?;
        assert!(!output.status.success(), "unused relay option was accepted");
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(document["command_status"], "error");
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(diagnostic.contains("--nostrconnect") && diagnostic.contains("--no-activate"));
        assert_eq!(std::fs::read(&config_path)?, config_before);
        assert!(
            std::fs::read(credentials.path())? == credentials_before,
            "refused login changed credentials"
        );
    }
    Ok(())
}
