//! ngit's *global* login scope must land in the redirected global Git
//! config, never in the invoking user's real one.
//!
//! Two separate mechanisms have to hold for that to be true, and this file
//! exercises both against a real `ngit account login`:
//!
//! 1. ngit resolves `GIT_CONFIG_GLOBAL` itself. libgit2 does not:
//!    `git_config_open_default()` ignores the variable outright, and
//!    `config_path_global()` honours it only for repositories opened with
//!    `GIT_REPOSITORY_OPEN_FROM_ENV`, which `git2::Repository::discover` —
//!    behind `Repo::discover` — does not use.
//! 2. The harness sandboxes `HOME` and the XDG base dirs, so even the fallback
//!    resolution stays inside the test.
//!
//! Without both, a test that drops `NGITTEST` (which is what re-enables
//! ngit's global scope) rewrites the developer's `~/.gitconfig`: this is
//! exactly how `tests/account_whoami.rs` used to delete a real login and
//! leave its throwaway account behind.

use anyhow::{Context, Result};
use nostr::prelude::{Keys, ToBech32};
use serde_json::Value;
use test_harness::Harness;

#[tokio::test]
async fn global_login_writes_to_the_redirected_global_config_only() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let redirected = tempfile::tempdir()?;
    let global_config = redirected.path().join("gitconfig");
    std::fs::write(&global_config, "")?;

    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;

    // No `--local`: this is the global login path, and removing NGITTEST is
    // what lets ngit act on the global scope at all.
    let output = repo
        .ngit(["account", "login", "--offline", "--nsec", &nsec])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await
        .context("failed to spawn ngit account login")?;
    assert!(
        output.status.success(),
        "global login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        read_config(&global_config, "nostr.nsec")?.as_deref(),
        Some(nsec.as_str()),
        "the global login must land in GIT_CONFIG_GLOBAL"
    );

    // The sandbox `HOME` stands in for the developer's machine: nothing may
    // reach it, through either the `$HOME/.gitconfig` or the XDG search.
    for escaped in [
        harness.home().join(".gitconfig"),
        harness.home().join(".config/git/config"),
    ] {
        assert_eq!(
            read_config(&escaped, "nostr.nsec")?,
            None,
            "a global login escaped to {}",
            escaped.display()
        );
    }
    Ok(())
}

#[tokio::test]
async fn global_logout_leaves_the_invoking_users_login_alone() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let redirected = tempfile::tempdir()?;
    let global_config = redirected.path().join("gitconfig");

    // Two logins: one in the redirected global config the command is
    // pointed at, one standing in for the developer's own machine.
    let redirected_nsec = Keys::generate().secret_key().to_bech32()?;
    let home_nsec = Keys::generate().secret_key().to_bech32()?;
    let home_config = harness.home().join(".gitconfig");
    write_config(&global_config, "nostr.nsec", &redirected_nsec)?;
    write_config(&home_config, "nostr.nsec", &home_nsec)?;

    let output = repo
        .ngit(["account", "logout"])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await
        .context("failed to spawn ngit account logout")?;
    assert!(
        output.status.success(),
        "global logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        read_config(&global_config, "nostr.nsec")?,
        None,
        "logout must clear the login in GIT_CONFIG_GLOBAL"
    );
    assert_eq!(
        read_config(&home_config, "nostr.nsec")?.as_deref(),
        Some(home_nsec.as_str()),
        "logout reached the invoking user's own global config"
    );
    Ok(())
}

#[tokio::test]
async fn global_signer_from_a_gitdir_conditional_include_is_visible() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let global_config = harness.home().join(".gitconfig");
    let included_config = harness.home().join("conditional.gitconfig");
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let nsec = keys.secret_key().to_bech32()?;

    for (key, value) in [
        ("nostr.signer", "conditional"),
        ("nostr.signer-alias.conditional", npub.as_str()),
        ("nostr.npub", npub.as_str()),
        ("nostr.nsec", nsec.as_str()),
    ] {
        write_config(&included_config, key, value)?;
    }
    std::fs::write(
        &global_config,
        format!(
            "[includeIf \"gitdir/i:{}/**\"]\n\tpath = {}\n",
            repo.dir().display(),
            included_config.display()
        ),
    )?;

    for use_global_override in [false, true] {
        let mut command = repo.ngit(["account", "whoami", "--offline", "--json"]);
        command.env_remove("NGITTEST");
        if !use_global_override {
            command.env_remove("GIT_CONFIG_GLOBAL");
        }
        let output = command
            .output()
            .await
            .context("failed to spawn ngit account whoami")?;
        assert!(
            output.status.success(),
            "account whoami failed with global override {use_global_override}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let document: Value = serde_json::from_slice(&output.stdout)?;
        let account = document["accounts"]
            .as_array()
            .context("accounts must be an array")?
            .iter()
            .find(|account| account["npub"] == npub)
            .with_context(|| {
                format!(
                    "conditionally included account is missing with global override {use_global_override}"
                )
            })?;
        assert_eq!(account["active"], true);
        assert_eq!(account["scopes"], serde_json::json!(["global"]));
        assert_eq!(account["aliases"], serde_json::json!(["conditional"]));
    }
    Ok(())
}

/// Write one key straight to a config file, without inheriting any scope.
fn write_config(path: &std::path::Path, key: &str, value: &str) -> Result<()> {
    git2::Config::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .set_str(key, value)
        .with_context(|| format!("failed to set {key} in {}", path.display()))
}

/// Read one key straight from a config file, without inheriting any scope.
fn read_config(path: &std::path::Path, key: &str) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let config =
        git2::Config::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    Ok(config.get_string(key).ok())
}
