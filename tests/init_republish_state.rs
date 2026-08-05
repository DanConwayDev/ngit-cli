//! Repeat `ngit init` with a cached kind-30618: the cached ref values
//! are re-signed as a fresh state event and established through the
//! state transaction.
//!
//! The republish exists so that relays (and git servers) newly added to
//! the announcement receive the repository state immediately instead of
//! waiting for the next real `git push` (originally introduced by
//! 732567f, restored after 489b5c8 removed the eager-publish variant).
//! Two properties are pinned here:
//!
//! 1. **Fresh event, identical refs, newly added relay covered** — a repeat
//!    init that adds a `--relay` publishes a kind-30618 with a *new* event id
//!    but the *same* ref map, and the newly announced relay holds it even
//!    though it was never in ngit's default relay set (the `extra` role is
//!    deliberately not env-injected).
//! 2. **Failure leaves the previous state authoritative** — when the push
//!    cannot establish the fresh event (no git server listable), init exits
//!    non-zero pointing at `ngit sync` as the follow-up and no new kind-30618
//!    reaches any relay. (The cache-write half of the invariant — commit only
//!    after acceptance — is unit-tested on the transaction driver in
//!    `state_transaction.rs`.)
//!
//! Assertions run immediately after each `ngit init` subprocess exits:
//! init publishes its events in-process before returning, so there is
//! nothing asynchronous left to poll for.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::Harness;

const DISPLAY_NAME: &str = "Republish Project";
/// `ngit init` slugifies `--name` into the `d` tag by replacing spaces
/// with hyphens.
const EXPECTED_IDENTIFIER: &str = "Republish-Project";

/// The state event's ref view: every tag whose name slot is a full ref
/// path (plus `HEAD`), mapped to its value slot. Ignores ordering and
/// non-ref tags (`d`, ordering nonces, ...), which legitimately differ
/// between two signings of the same state.
fn ref_map(event: &Event) -> std::collections::BTreeMap<String, String> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            let name = s.first()?;
            if name.starts_with("refs/") || name == "HEAD" {
                Some((name.clone(), s.get(1).cloned().unwrap_or_default()))
            } else {
                None
            }
        })
        .collect()
}

/// The single kind-30618 for this test's identifier currently held by a
/// relay (30618 is addressable, so a relay retains only the latest per
/// coordinate).
async fn state_event_on(
    relay: &test_harness::VanillaRelay,
    author: PublicKey,
) -> Result<Option<Event>> {
    let events = relay
        .events(Filter::new().author(author).kind(Kind::Custom(30618)))
        .await?;
    let mut matching: Vec<Event> = events
        .into_iter()
        .filter(|e| {
            e.tags.iter().any(|t| {
                let s = t.as_slice();
                s.first().map(String::as_str) == Some("d")
                    && s.get(1).map(String::as_str) == Some(EXPECTED_IDENTIFIER)
            })
        })
        .collect();
    if matching.len() > 1 {
        bail!(
            "expected at most one kind-30618 per coordinate on a relay, got {}",
            matching.len()
        );
    }
    Ok(matching.pop())
}

async fn run_init(repo: &test_harness::Repo, args: &[&str]) -> Result<std::process::Output> {
    let mut full = vec!["init", "--name", DISPLAY_NAME];
    full.extend_from_slice(args);
    repo.ngit(full)
        .output()
        .await
        .context("failed to spawn ngit init")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_init_republishes_fresh_state_to_newly_added_relay() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    // Deliberately not a role ngit's env knows about: the only way this
    // relay can receive the state event is via the announcement.
    .with_relay("extra")
    .with_vanilla_git_server("host")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let pubkey = state.keys.public_key();
    let vanilla_url = harness.vanilla_git_server("host").url().to_string();
    let default_relay_url = harness.relay("default").url().to_string();
    let extra_relay_url = harness.relay("extra").url().to_string();

    // First init: fresh repo, no origin — pushes main to the vanilla
    // server and publishes the first kind-30618 to the announced relay.
    let first = run_init(
        &repo,
        &["--clone", &vanilla_url, "--relay", &default_relay_url],
    )
    .await?;
    if !first.status.success() {
        bail!(
            "first ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            first.status,
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr),
        );
    }
    let first_event = state_event_on(harness.relay("default"), pubkey)
        .await?
        .context("no kind-30618 on the default relay after the first init")?;
    assert!(
        state_event_on(harness.relay("extra"), pubkey)
            .await?
            .is_none(),
        "the extra relay must not hold the state before it is announced",
    );

    // Repeat init announcing an additional relay. The refs are
    // unchanged, so only the cached-state republish can bring the state
    // event to the newly announced relay.
    let second = run_init(
        &repo,
        &[
            "--clone",
            &vanilla_url,
            "--relay",
            &default_relay_url,
            "--relay",
            &extra_relay_url,
        ],
    )
    .await?;
    if !second.status.success() {
        bail!(
            "repeat ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            second.status,
            String::from_utf8_lossy(&second.stdout),
            String::from_utf8_lossy(&second.stderr),
        );
    }

    let republished = state_event_on(harness.relay("extra"), pubkey)
        .await?
        .context(
            "no kind-30618 on the newly announced relay after the repeat \
             init — the cached-state republish should have fanned the fresh \
             event out to every announced relay",
        )?;
    assert_ne!(
        republished.id, first_event.id,
        "the repeat init must publish a fresh state event, not re-send the \
         cached one",
    );
    assert_eq!(
        ref_map(&republished),
        ref_map(&first_event),
        "the republished state must carry the same ref values as the cached \
         state it re-signs",
    );

    // The fresh event is ordered after its predecessor, so the original
    // relay replaces its copy too.
    let on_default = state_event_on(harness.relay("default"), pubkey)
        .await?
        .context("kind-30618 disappeared from the default relay")?;
    assert_eq!(
        on_default.id, republished.id,
        "the previously announced relay should hold the fresh state event \
         (NIP-01 replacement of the older one)",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_republish_leaves_previous_state_authoritative() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("host")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let pubkey = state.keys.public_key();
    let vanilla_url = harness.vanilla_git_server("host").url().to_string();
    let default_relay_url = harness.relay("default").url().to_string();

    let first = run_init(
        &repo,
        &["--clone", &vanilla_url, "--relay", &default_relay_url],
    )
    .await?;
    if !first.status.success() {
        bail!(
            "first ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            first.status,
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr),
        );
    }
    let first_event = state_event_on(harness.relay("default"), pubkey)
        .await?
        .context("no kind-30618 on the default relay after the first init")?;

    // A port that was just bound and released: connecting is refused, so
    // listing the "git server" fails fast and no server is listable.
    let dead_url = {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").context("failed to reserve a dead port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        format!("http://127.0.0.1:{port}/repo.git")
    };

    let failed = run_init(
        &repo,
        &["--clone", &dead_url, "--relay", &default_relay_url],
    )
    .await?;
    assert!(
        !failed.status.success(),
        "repeat init with no listable git server must fail instead of \
         broadcasting a state event no git server holds\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr),
    );
    // Explicit error-message contract from the phase-3 init changes: the
    // failure names the follow-up command.
    let stderr = String::from_utf8_lossy(&failed.stderr).to_lowercase();
    assert!(
        stderr.contains("ngit sync"),
        "failed republish should point at `ngit sync` as the follow-up; \
         stderr: {stderr}",
    );

    // No fresh state event was broadcast: the relay still holds exactly
    // the event the successful init published.
    let on_default = state_event_on(harness.relay("default"), pubkey)
        .await?
        .context("kind-30618 disappeared from the default relay")?;
    assert_eq!(
        on_default.id, first_event.id,
        "a failed republish must leave the previously published state event \
         authoritative on the relays",
    );

    Ok(())
}
