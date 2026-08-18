//! Enhanced account-whoami inventory and signer-guidance coverage.

use anyhow::{Context, Result};
use serde_json::Value;
use tempfile::NamedTempFile;
use test_harness::{Harness, repo::Repo};

async fn create_stored_account(
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
        "account creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let npub = repo
        .config("nostr.npub")
        .await?
        .context("created account has no configured npub")?;
    logout(repo, credentials).await?;
    Ok(npub)
}

async fn activate_local_alias(
    repo: &Repo,
    credentials: &NamedTempFile,
    npub: &str,
    alias: &str,
) -> Result<()> {
    let output = repo
        .ngit([
            "account",
            "login",
            npub,
            "--local",
            "--offline",
            "--alias",
            alias,
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "local alias activation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn logout(repo: &Repo, credentials: &NamedTempFile) -> Result<()> {
    let output = repo
        .ngit(["account", "logout"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "account logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn account_by_npub<'a>(document: &'a Value, npub: &str) -> &'a Value {
    document["accounts"]
        .as_array()
        .expect("accounts must be an array")
        .iter()
        .find(|account| account["npub"] == npub)
        .expect("listed account missing")
}

fn string_array(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .expect("value must be an array")
        .iter()
        .map(|item| item.as_str().expect("array item must be a string"))
        .collect()
}

#[tokio::test]
async fn account_whoami_is_empty_when_no_signer_is_available() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;

    let output = repo
        .ngit(["account", "whoami", "--offline", "--json"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "empty account whoami failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["accounts"].as_array().map(Vec::len), Some(0));
    Ok(())
}

#[tokio::test]
async fn account_whoami_combines_retained_local_and_global_signers() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;
    let global_dir = tempfile::tempdir()?;
    let global_config = global_dir.path().join("gitconfig");
    let production_cache = tempfile::tempdir()?;
    let global_login_cache = tempfile::tempdir()?;
    std::fs::File::create(&global_config)?;

    let retained = create_stored_account(&repo, &credentials, "Lighthouse Alice").await?;
    activate_local_alias(&repo, &credentials, &retained, "lighthouse").await?;
    logout(&repo, &credentials).await?;

    let unaliased = create_stored_account(&repo, &credentials, "Plain Jane").await?;

    let globally_logged_in = create_stored_account(&repo, &credentials, "Global Ghost").await?;
    activate_local_alias(&repo, &credentials, &globally_logged_in, "globalghost").await?;
    logout(&repo, &credentials).await?;
    let output = repo
        .ngit([
            "account",
            "login",
            globally_logged_in.as_str(),
            "--offline",
            "--alias",
            "globalghost",
        ])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("NGIT_CACHE_DIR", global_login_cache.path())
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "global account activation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let locally_logged_in = create_stored_account(&repo, &credentials, "DanConwayDev").await?;
    activate_local_alias(&repo, &credentials, &locally_logged_in, "dcdev").await?;
    logout(&repo, &credentials).await?;
    activate_local_alias(&repo, &credentials, &locally_logged_in, "shipwright").await?;

    // Removing NGITTEST enables real global-config precedence, and also moves
    // the cache to its production location. Re-home the completed test cache
    // so the same cached profiles remain observable in that mode.
    std::fs::rename(
        repo.dir().join(".git/test-global-cache.lmdb"),
        production_cache.path().join("nostr-cache.lmdb"),
    )?;

    let output = repo
        .ngit(["account", "whoami", "--offline", "--json"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("NGIT_CACHE_DIR", production_cache.path())
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "account whoami failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["accounts"].as_array().map(Vec::len), Some(4));

    let local = account_by_npub(&document, &locally_logged_in);
    assert_eq!(local["name"], "DanConwayDev");
    assert_eq!(local["active"], true);
    assert_eq!(string_array(&local["scopes"]), ["local"]);
    assert_eq!(string_array(&local["aliases"]), ["dcdev", "shipwright"]);
    let local_selectors = local["selectors"]
        .as_array()
        .context("local selectors must be an array")?;
    for selector in ["DanConwayDev", "dcdev", "shipwright", &locally_logged_in] {
        assert!(
            local_selectors.iter().any(|item| item["value"] == selector),
            "missing direct signer guidance for {selector}"
        );
    }

    let global = account_by_npub(&document, &globally_logged_in);
    assert_eq!(global["active"], false);
    assert_eq!(string_array(&global["scopes"]), ["global"]);
    assert_eq!(string_array(&global["aliases"]), ["globalghost"]);

    let retained = account_by_npub(&document, &retained);
    assert_eq!(retained["active"], false);
    assert!(string_array(&retained["scopes"]).is_empty());
    assert_eq!(string_array(&retained["aliases"]), ["lighthouse"]);

    let unaliased = account_by_npub(&document, &unaliased);
    assert_eq!(unaliased["name"], "Plain Jane");
    assert!(string_array(&unaliased["aliases"]).is_empty());

    let list_alias = repo
        .ngit(["account", "list", "--offline", "--json"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("NGIT_CACHE_DIR", production_cache.path())
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        list_alias.status.success(),
        "account list alias failed: {}",
        String::from_utf8_lossy(&list_alias.stderr)
    );
    let list_document: Value = serde_json::from_slice(&list_alias.stdout)?;
    assert_eq!(list_document["accounts"].as_array().map(Vec::len), Some(4));

    let human = repo
        .ngit(["account", "whoami", "--offline"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("NGIT_CACHE_DIR", production_cache.path())
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        human.status.success(),
        "human account whoami failed: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human = String::from_utf8(human.stdout)?;
    let local_npub_line = format!("  npub: {locally_logged_in}");
    for expected in [
        local_npub_line.as_str(),
        "  aliases: dcdev, shipwright",
        "Plain Jane",
        "  aliases: none",
        "ACCOUNT can be any full npub or listed alias above",
        "or your exact Nostr\nprofile name.",
        "ngit --signer ACCOUNT <command>",
        "git -c nostr.signer=ACCOUNT <command>",
        "ngit account login ACCOUNT",
        "ngit account login --local ACCOUNT",
        "ngit account login ACCOUNT --alias ALIAS",
        "ngit account logout",
        "activate global Global Ghost",
    ] {
        assert!(
            human.contains(expected),
            "missing human guidance: {expected}"
        );
    }
    assert_eq!(human.matches("commands:").count(), 1);
    assert!(!human.contains("  use:"));

    // The two friendliest suggestions are not decorative: each resolves via
    // the same one-shot signer path without changing the configured login.
    for selector in ["DanConwayDev", "dcdev"] {
        let output = repo
            .ngit(["--signer", selector, "account", "export-keys", "--json"])
            .env_remove("NGITTEST")
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("NGIT_CACHE_DIR", production_cache.path())
            .env("NGIT_SECRET_STORAGE", "file")
            .env("NGIT_KEYRING_FILE", credentials.path())
            .output()
            .await?;
        assert!(
            output.status.success(),
            "suggested selector {selector} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some("shipwright")
    );
    Ok(())
}
