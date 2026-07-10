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
use nostr_sdk::prelude::*;
use test_harness::Harness;

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
