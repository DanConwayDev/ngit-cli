//! `ngit init` State A "Fresh" — successor to legacy
//! `tests/legacy/ngit_init.rs::state_a_fresh::*`.
//!
//! State A is the absence of a `nostr.repo` git config: a brand-new repo
//! that has never been associated with a Nostr coordinate.
//! `validate_pre_fetch` (init.rs:472-475) therefore takes the State A
//! short-circuit and runs `validate_fresh` (init.rs:342) before any
//! network round-trip — so error cases here are observable without any
//! relay or grasp interaction having to happen, and the success case is
//! a pure announcement publish.
//!
//! ## Coverage
//!
//! - **Errors** (3 tests, no shared setup — each error is a different `ngit
//!   init` invocation with its own arg shape):
//!   - `bare_no_flags` → "missing required fields"
//!   - `name_only_missing_server_infra` → "missing --grasp-server"
//!   - `relays_only_missing_name_and_servers` → "missing required fields" (the
//!     "two missing" branch)
//! - **Success** (1 captured snapshot, 7 rstest cases asserting on different
//!   tags of the produced kind-30617). Setup runs once per test binary via
//!   [`tokio::sync::OnceCell`]; every case is a read-only assertion on the
//!   captured `Snapshot`. Same discipline as `tests/send_patch.rs` and
//!   `tests/git_push_state/fresh_repo.rs` — see those files' module-level docs
//!   for the rationale.
//!
//! ## Error-message brittleness
//!
//! The error tests assert on `ngit`'s stderr containing a specific
//! substring (e.g. `"missing required fields"`). The harness rules ban
//! *exact*-stdout assertions; substring assertions on a stable error-
//! prefix are tolerated as a regression-catching shortcut for tests
//! whose entire contract *is* "this validation arm fired". The strings
//! are produced by `cli_error` in `src/bin/ngit/sub_commands/init.rs`
//! and have not changed since the legacy tests were written; if init
//! starts wording the messages differently, these tests will fail loudly
//! and the assertions can be updated in the same change.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::Harness;
use tokio::sync::OnceCell;

const DISPLAY_NAME: &str = "My Project";
/// `ngit init` slugifies `--name` into the `d` tag by replacing spaces
/// with hyphens — `"My Project"` → `"My-Project"`. The producer side
/// lives in `src/bin/ngit/sub_commands/init.rs`'s identifier-derivation
/// logic. Captured here so a regression in either direction (missing
/// slugification, different separator) shows up as a tag mismatch.
const EXPECTED_IDENTIFIER: &str = "My-Project";

// ---------------------------------------------------------------------------
// Errors — one #[tokio::test] each, no shared setup
// ---------------------------------------------------------------------------

/// Equivalent of legacy `state_a_fresh::errors::bare_no_flags`. Bare
/// `ngit init` against a fresh repo trips `validate_fresh`'s "two
/// missing" branch (no name + no grasp), surfacing as
/// "missing required fields".
#[tokio::test]
async fn bare_no_flags_errors_missing_required_fields() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_a_fresh().await?;
    let out = repo
        .ngit(["init"])
        .output()
        .await
        .context("failed to spawn ngit init")?;

    assert!(
        !out.status.success(),
        "expected `ngit init` to fail with no flags in State A; \
         exited successfully\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("missing required fields"),
        "expected 'missing required fields' error, got: {combined}",
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_a_fresh::errors::name_only_missing_server_infra`. With only
/// `--name` provided, `validate_fresh` falls into the "one missing"
/// branch — the message names the specific missing flag rather than
/// the umbrella "missing required fields".
#[tokio::test]
async fn name_only_errors_missing_grasp_server() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_a_fresh().await?;
    let out = repo
        .ngit(["init", "--name", DISPLAY_NAME])
        .output()
        .await
        .context("failed to spawn ngit init --name")?;

    assert!(
        !out.status.success(),
        "expected `ngit init --name` to fail in State A; exited successfully",
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("missing --grasp-server"),
        "expected 'missing --grasp-server' error, got: {combined}",
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_a_fresh::errors::relays_only_missing_name_and_servers`. With
/// only `--relay` provided (and no `--clone` / `--grasp-server`),
/// `validate_fresh` lists *both* missing flags and falls back to the
/// umbrella message.
#[tokio::test]
async fn relays_only_errors_missing_required_fields() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_a_fresh().await?;
    // Use the harness's default relay URL rather than a hard-coded
    // localhost port: any reachable ws URL satisfies the `--relay`
    // *parser*, and we want the test's success path to depend only on
    // hitting the validate_fresh "two missing" branch — not on whether
    // a particular hardcoded relay happens to be alive.
    let relay_url = harness.relay("default").url().to_string();
    let out = repo
        .ngit(["init", "--relay", &relay_url])
        .output()
        .await
        .context("failed to spawn ngit init --relay")?;

    assert!(
        !out.status.success(),
        "expected `ngit init --relay <url>` to fail in State A; exited successfully",
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("missing required fields"),
        "expected 'missing required fields' error, got: {combined}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Success — shared snapshot, one rstest function per asserted property
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --name "My Project"
/// --grasp-server <url>` invocation against a State A repo.
///
/// Holds the announcement event itself (rather than pre-extracted tag
/// values) so future cases asserting on additional tags don't have to
/// re-run setup; the maintainer pubkey + grasp URL prefix are surfaced
/// alongside because more than one case asserts on them.
struct Snapshot {
    announcement: Event,
    maintainer_pubkey: PublicKey,
    /// `http://127.0.0.1:<port>` — the grasp's URL the test passed to
    /// `--grasp-server`. Cloned URLs in the announcement should start
    /// with this prefix (and end with `/<npub>/<identifier>.git`).
    grasp_http_url: String,
    /// `ws://127.0.0.1:<port>` — the grasp's relay endpoint, expected
    /// to be one of the values in the announcement's `relays` tag.
    grasp_relay_url: String,
    /// Maintainer's npub — used by the `clone url contains npub` case.
    maintainer_npub: String,
    /// Root commit oid captured during arrange. The `r euc` tag on the
    /// announcement should equal this.
    root_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("init_state_fresh fixture: capture_snapshot failed"),
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

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;

    let grasp = harness.grasp("repo");
    let grasp_http_url = grasp.url().to_string();
    let grasp_relay_url = grasp.relay_url();

    // No `--identifier`, no `--description` — match legacy
    // `state_a_fresh::success::with_name_and_grasp_server`'s shape so
    // identifier-derivation, default-empty-description, etc. are all
    // exercised exactly as legacy did.
    let init_out = repo
        .ngit([
            "init",
            "--name",
            DISPLAY_NAME,
            "--grasp-server",
            &grasp_http_url,
        ])
        .output()
        .await
        .context("failed to spawn ngit init --name --grasp-server")?;
    if !init_out.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    // Query the **vanilla default relay** for the announcement, not
    // the grasp: `ngit-grasp` routes new announcements to purgatory
    // until git data arrives, and under `NGITTEST=TRUE` the post-init
    // `git push` is short-circuited (init.rs:1195) — see
    // `tests/init_grasp.rs`'s module-level doc-comment for the chain.
    // The default relay always materialises the kind-30617 because
    // ngit fans out to the user's relay-list on publish.
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
        .find(|e| tag_value(e, "d").as_deref() == Some(EXPECTED_IDENTIFIER))
        .with_context(|| {
            format!(
                "no kind-30617 with `d` = {EXPECTED_IDENTIFIER:?} on the default \
                 relay after `ngit init --name`"
            )
        })?;

    Ok(Snapshot {
        announcement,
        maintainer_pubkey: state.keys.public_key(),
        grasp_http_url,
        grasp_relay_url,
        maintainer_npub: state.npub,
        root_oid: state.root_oid,
    })
}

/// Equivalent of legacy
/// `with_name_and_grasp_server::identifier_derived_from_name`. The `d`
/// tag is the slug-cased `--name` argument.
#[rstest]
#[tokio::test]
async fn identifier_derived_from_name(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "d").as_deref(),
        Some(EXPECTED_IDENTIFIER),
    );
    Ok(())
}

/// Equivalent of legacy `with_name_and_grasp_server::name_tag_matches`.
/// The `name` tag carries the raw (un-slugified) `--name` argument.
#[rstest]
#[tokio::test]
async fn name_tag_matches(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "name").as_deref(),
        Some(DISPLAY_NAME)
    );
    Ok(())
}

/// Equivalent of legacy `with_name_and_grasp_server::description_empty`.
/// No `--description` was passed, so the `description` tag is the empty
/// string (the producer always emits the tag, even when blank — see
/// `RepoRef::to_event`).
#[rstest]
#[tokio::test]
async fn description_empty(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "description").as_deref(),
        Some("")
    );
    Ok(())
}

/// Equivalent of legacy
/// `with_name_and_grasp_server::clone_url_derived_from_grasp_server`.
/// Three sub-properties of the same `clone` tag:
///
/// - exactly one URL emitted (`--grasp-server` was passed once, and no
///   `--clone` was added),
/// - URL starts with the grasp's HTTP base,
/// - URL ends with `/<identifier>.git`,
/// - URL contains the maintainer's npub (the `<git_data_path>/<npub>/...`
///   layout that ngit-grasp's announcement policy provisions).
#[rstest]
#[tokio::test]
async fn clone_url_derived_from_grasp_server(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let clone_urls = tag_values(&s.announcement, "clone");
    assert_eq!(
        clone_urls.len(),
        1,
        "expected exactly one clone url; got {clone_urls:?}",
    );
    let url = &clone_urls[0];
    assert!(
        url.starts_with(&format!("{}/", s.grasp_http_url)),
        "clone url should start with grasp HTTP base ({}/); got: {url}",
        s.grasp_http_url,
    );
    assert!(
        url.ends_with(&format!("/{EXPECTED_IDENTIFIER}.git")),
        "clone url should end with /{EXPECTED_IDENTIFIER}.git; got: {url}",
    );
    assert!(
        url.contains(&s.maintainer_npub),
        "clone url should contain maintainer npub ({}); got: {url}",
        s.maintainer_npub,
    );
    Ok(())
}

/// Equivalent of legacy
/// `with_name_and_grasp_server::relays_include_grasp_derived`. The
/// announcement's `relays` tag includes the grasp's ws URL (added by
/// `apply_grasp_infrastructure` in `src/lib/repo_ref.rs:836`).
#[rstest]
#[tokio::test]
async fn relays_include_grasp_derived(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let relays = tag_values(&s.announcement, "relays");
    assert!(
        relays.iter().any(|r| r == &s.grasp_relay_url),
        "relays should include grasp-derived ws url ({}); got {relays:?}",
        s.grasp_relay_url,
    );
    Ok(())
}

/// Equivalent of legacy
/// `with_name_and_grasp_server::maintainers_is_just_me`. With no
/// `--other-maintainers`, the announcement lists only the publishing
/// pubkey.
#[rstest]
#[tokio::test]
async fn maintainers_is_just_me(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let maintainers = tag_values(&s.announcement, "maintainers");
    assert_eq!(
        maintainers.len(),
        1,
        "expected single maintainer; got {maintainers:?}",
    );
    assert_eq!(
        maintainers[0],
        s.maintainer_pubkey.to_string(),
        "expected sole maintainer to be the publisher",
    );
    Ok(())
}

/// Equivalent of legacy
/// `with_name_and_grasp_server::earliest_unique_commit_is_root`. The
/// `r ... euc` tag value matches the captured root commit oid — caught
/// dynamically against the arrange's `root_oid` rather than the
/// hardcoded `9ee507fc...` baked into legacy
/// `test_utils::generate_repo_ref_event`.
#[rstest]
#[tokio::test]
async fn earliest_unique_commit_is_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let euc = s
        .announcement
        .tags
        .iter()
        .find_map(|t| {
            let parts = t.as_slice();
            if parts.first().map(String::as_str) == Some("r")
                && parts.len() > 2
                && parts.get(2).map(String::as_str) == Some("euc")
            {
                parts.get(1).cloned()
            } else {
                None
            }
        })
        .context("announcement missing the `r <oid> euc` tag")?;
    assert_eq!(euc, s.root_oid);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// First value of the first tag whose name slot equals `key`, if any.
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

/// All values (slot 1+) of every tag whose name slot equals `key`. Used
/// for multi-value tags (`clone`, `relays`, `maintainers`).
fn tag_values(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(key))
        .map(|t| t.as_slice()[1..].to_vec())
        .unwrap_or_default()
}
