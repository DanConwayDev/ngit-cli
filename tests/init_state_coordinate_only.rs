//! `ngit init` State B "CoordinateOnly" — successor to legacy
//! `tests/legacy/ngit_init.rs::state_b_coordinate_only::*`.
//!
//! State B is a `nostr.repo` git config that points at a coordinate
//! (kind:pubkey:identifier triple) that **no relay carries an
//! announcement for**. ngit's lookup completes the network round-trip,
//! finds nothing, and `validate_post_fetch` (init.rs:515-530) trips the
//! `CoordinateOnly` arm — bare `ngit init` is rejected unless `--force`
//! is set, even with `--defaults`.
//!
//! ## Coverage
//!
//! - **Errors** (2 tests, no shared setup):
//!   - `bare_no_flags_errors_no_announcement` → "no announcement found for
//!     coordinate"
//!   - `defaults_still_errors_no_announcement` → same error even with
//!     `--defaults`
//! - **Success** (1 captured snapshot, 3 rstest cases asserting on the
//!   `--force` path's announcement). Setup is shared via
//!   [`tokio::sync::OnceCell`] — same discipline as `tests/init_state_fresh.rs`
//!   and `tests/send_patch.rs`.
//!
//! ## Why two arrange invocations in errors
//!
//! Each error case uses its own
//! [`Harness::arrange_init_state_b_coordinate_only`] call rather than sharing
//! one. Setup is cheap (account create + seed commits + a single `git config`
//! write), and giving each error case its own harness keeps assertion noise
//! isolated when one fails. Sharing would require the success snapshot's `Arc`
//! discipline for tests that don't actually need it.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::Harness;
use tokio::sync::OnceCell;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Equivalent of legacy
/// `state_b_coordinate_only::errors::bare_no_flags`. Bare `ngit init`
/// in State B fails post-fetch with the CoordinateOnly arm's error.
#[tokio::test]
async fn bare_no_flags_errors_no_announcement() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_b_coordinate_only().await?;
    let out = repo
        .ngit(["init"])
        .output()
        .await
        .context("failed to spawn ngit init")?;

    assert!(
        !out.status.success(),
        "expected `ngit init` to fail in State B; exited successfully\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("no announcement found for coordinate"),
        "expected coordinate-only error, got: {combined}",
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_b_coordinate_only::errors::defaults_still_requires_force`.
/// `--defaults` doesn't bypass the CoordinateOnly check — only
/// `--force` does. The test fixes the regression where `-d` accidentally
/// short-circuited the announcement-lookup error.
#[tokio::test]
async fn defaults_still_errors_no_announcement() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_b_coordinate_only().await?;
    let out = repo
        .ngit(["init", "--defaults"])
        .output()
        .await
        .context("failed to spawn ngit init --defaults")?;

    assert!(
        !out.status.success(),
        "expected `ngit init --defaults` to fail in State B; exited successfully",
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("no announcement found for coordinate"),
        "expected coordinate-only error even with --defaults, got: {combined}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Success — `--force --grasp-server <url>` path
// ---------------------------------------------------------------------------

/// Captured side-effects of one
/// `ngit init --force --grasp-server <url>` invocation against a
/// State B repo. Three assertion cases share this snapshot via
/// `OnceCell`; setup runs once per test binary.
struct Snapshot {
    announcement: Event,
    /// Identifier the State B arrange wrote into `nostr.repo` — the
    /// `d` tag on the post-init announcement should equal this. Same
    /// value also pins the `name` default-fallback assertion.
    coordinate_identifier: String,
    /// Grasp HTTP base — clone URLs in the announcement should start
    /// with this.
    grasp_http_url: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("init_state_coordinate_only fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

async fn capture_snapshot() -> Result<Snapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_b_coordinate_only().await?;

    let grasp = harness.grasp("repo");
    let grasp_http_url = grasp.url().to_string();

    // `--force` to bypass the CoordinateOnly arm; `--grasp-server` to
    // satisfy `validate_fresh`'s server-infra requirement (see
    // init.rs:361-370). No `--name` / `--identifier` so the
    // identifier-from-coordinate inheritance is exercised.
    let init_out = repo
        .ngit(["init", "--force", "--grasp-server", &grasp_http_url])
        .output()
        .await
        .context("failed to spawn ngit init --force --grasp-server")?;
    if !init_out.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    // Same query-the-default-relay strategy as `init_state_fresh.rs` —
    // the grasp routes new announcements to purgatory until git data
    // arrives, so REQs against it return nothing under
    // `NGITTEST=TRUE` (see `tests/init_grasp.rs`'s module doc).
    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(state.keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .into_iter()
        .find(|e| tag_value(e, "d").as_deref() == Some(state.coordinate_identifier.as_str()))
        .with_context(|| {
            format!(
                "no kind-30617 with `d` = {:?} on the default relay after \
                 `ngit init --force`",
                state.coordinate_identifier,
            )
        })?;

    Ok(Snapshot {
        announcement,
        coordinate_identifier: state.coordinate_identifier,
        grasp_http_url,
    })
}

/// Equivalent of legacy
/// `state_b_coordinate_only::success::identifier_from_coordinate`. With
/// no `--identifier` flag, `ngit init` inherits the existing
/// coordinate's identifier rather than minting a new one — pinning the
/// "the same repo, just re-announced" semantics.
#[rstest]
#[tokio::test]
async fn identifier_from_coordinate(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "d").as_deref(),
        Some(s.coordinate_identifier.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_b_coordinate_only::success::name_defaults_to_identifier`.
/// With no `--name` flag and no prior announcement to inherit from, the
/// `name` tag falls back to the identifier.
#[rstest]
#[tokio::test]
async fn name_defaults_to_identifier(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "name").as_deref(),
        Some(s.coordinate_identifier.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_b_coordinate_only::success::clone_url_from_grasp_server`.
/// At least one of the announcement's `clone` tag values starts with
/// the grasp's HTTP base — confirms the `--grasp-server` flag flowed
/// through to the clone-URL synthesis even when the rest of the
/// announcement was inherited from the coordinate.
#[rstest]
#[tokio::test]
async fn clone_url_from_grasp_server(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let clone_urls = tag_values(&s.announcement, "clone");
    let prefix = format!("{}/", s.grasp_http_url);
    assert!(
        clone_urls.iter().any(|u| u.starts_with(&prefix)),
        "expected at least one clone url starting with {prefix}; got {clone_urls:?}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — duplicated from init_state_fresh.rs because each integration
// test compiles as its own binary; sharing would require a fixtures
// crate or `mod common`, both of which are out of scope for PR 6a.
// ---------------------------------------------------------------------------

fn tag_value(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

fn tag_values(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(key))
        .map(|t| t.as_slice()[1..].to_vec())
        .unwrap_or_default()
}
