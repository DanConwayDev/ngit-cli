//! `ngit init` State C "MyAnnouncement" — successor to legacy
//! `tests/legacy/ngit_init.rs::state_c_my_announcement::*`.
//!
//! State C is the post-fetch arm in `validate_post_fetch`
//! (init.rs:531-549) where `repo_coordinate.pubkey == user_pubkey` and
//! an announcement signed by the user already exists on the
//! coordinate's relays. Bare `ngit init` is rejected with
//! `"no arguments specified, use --force to publish with new
//! timestamp"`; `--identifier <new>` is rejected with `"changing
//! identifier creates a new repository"` unless `--force` is also
//! passed; otherwise `ngit init` republishes the announcement with a
//! fresh `created_at`, preserving the existing tags unless overridden
//! explicitly on the CLI.
//!
//! ## Coverage
//!
//! - **Errors** (2 tests, no shared setup):
//!   - `bare_no_flags_errors_no_arguments_specified` → legacy
//!     `bare_no_flags_requires_force`
//!   - `identifier_change_errors_creates_new_repo` → legacy
//!     `identifier_change_requires_force`
//! - **Force-refresh success** (1 captured snapshot, 4 rstest cases asserting
//!   on different tags of the republished announcement). Setup runs `ngit init
//!   --force` once via [`tokio::sync::OnceCell`]; each case is a read-only
//!   assertion on the captured `Snapshot`.
//!   - `force_refresh_preserves_name` → legacy `name_preserved`
//!   - `force_refresh_preserves_description` → legacy `description_preserved`
//!   - `force_refresh_preserves_marker_relay` → legacy `relays_from_my_event`
//!   - `force_refresh_preserves_maintainers` → legacy `maintainers_preserved`
//! - **Name-override success** (1 captured snapshot, 2 rstest cases).
//!   - `name_override_replaces_name` → legacy `name_overridden`
//!   - `name_override_keeps_identifier` → legacy `identifier_unchanged`
//!
//! ## Why two snapshots
//!
//! `--force` and `--name "New Name"` exercise the same MyAnnouncement
//! re-publish path but with different CLI shapes; the resulting
//! announcements differ on the assertions each fixture covers. Sharing
//! one snapshot would require merging both invocations into a single
//! `ngit init` call that doesn't exist. Two `OnceCell` fixtures, one
//! per CLI shape, match the legacy `force_refresh` / `name_override`
//! `mod`-level fixture split.
//!
//! ## Error-message brittleness
//!
//! Same caveat as `tests/init_state_fresh.rs` and
//! `tests/init_state_coordinate_only.rs`: the error tests substring-
//! match on `cli_error`'s output. Substring assertions on stable error
//! prefixes are tolerated as a regression-catching shortcut for tests
//! whose entire contract *is* "this validation arm fired"; exact-stdout
//! is forbidden by the harness rules. If init starts wording the
//! messages differently, these tests fail loudly and the assertions
//! can be updated in the same change.

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
/// `state_c_my_announcement::errors::bare_no_flags_requires_force`. With
/// no substantive flags and no `--force`, the MyAnnouncement arm in
/// `validate_post_fetch` (init.rs:542-548) refuses to republish.
#[tokio::test]
async fn bare_no_flags_errors_no_arguments_specified() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_c_my_announcement().await?;
    let out = repo
        .ngit(["init"])
        .output()
        .await
        .context("failed to spawn ngit init")?;

    assert!(
        !out.status.success(),
        "expected `ngit init` to fail in State C; exited successfully\n\
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
        combined.contains("no arguments specified"),
        "expected 'no arguments specified' error, got: {combined}",
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_c_my_announcement::errors::identifier_change_requires_force`.
/// `--identifier <new>` without `--force` trips the
/// "changing identifier creates a new repository" arm (init.rs:481-489
/// pre-fetch, init.rs:532-540 post-fetch — pre-fetch catches it first
/// when a cached repo_ref exists, post-fetch catches it after the
/// network round-trip otherwise; either path surfaces the same
/// message).
#[tokio::test]
async fn identifier_change_errors_creates_new_repo() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_c_my_announcement().await?;
    let out = repo
        .ngit(["init", "--identifier", "new-id"])
        .output()
        .await
        .context("failed to spawn ngit init --identifier new-id")?;

    assert!(
        !out.status.success(),
        "expected `ngit init --identifier new-id` to fail in State C; \
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
        combined.contains("changing identifier creates a new repository"),
        "expected identifier-change error, got: {combined}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Success — `ngit init --force` (force-refresh fixture)
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --force` invocation against
/// a State C repo. Four assertion cases share this snapshot via
/// `OnceCell`; setup runs once per test binary.
struct ForceRefreshSnapshot {
    /// The kind-30617 event ngit republished. Newer than the
    /// arrange's `existing_announcement` (which had a back-dated
    /// `created_at`), so the relay's replaceable-event semantics
    /// surface this one on REQ.
    republished: Event,
    /// Name tag value carried by the *existing* announcement (the one
    /// the arrange published before `ngit init --force` ran).
    /// Asserted-equal to the republished announcement's `name` tag.
    existing_name: String,
    existing_description: String,
    /// Marker URL the arrange put into the existing announcement's
    /// `relays` tag — the one that's *not* the default relay, so its
    /// presence in the republished announcement proves ngit
    /// preserved the existing relay list (rather than re-deriving
    /// from its own default set).
    marker_relay_url: String,
    /// Publisher's pubkey.
    maintainer_pubkey: PublicKey,
    /// Co-maintainer pubkey (the one element of
    /// [`ArrangedInitStateC::additional_maintainer_keys`]). Asserted
    /// to appear alongside `maintainer_pubkey` in the republished
    /// announcement's `maintainers` tag.
    additional_maintainer_pubkey: PublicKey,
}

static FORCE_REFRESH_SNAPSHOT: OnceCell<Arc<ForceRefreshSnapshot>> = OnceCell::const_new();

#[fixture]
async fn force_refresh_snapshot() -> Arc<ForceRefreshSnapshot> {
    FORCE_REFRESH_SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_force_refresh()
                    .await
                    .expect("init_state_my_announcement: capture_force_refresh failed"),
            )
        })
        .await
        .clone()
}

async fn capture_force_refresh() -> Result<ForceRefreshSnapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_c_my_announcement().await?;

    let init_out = repo
        .ngit(["init", "--force"])
        .output()
        .await
        .context("failed to spawn ngit init --force")?;
    if !init_out.status.success() {
        bail!(
            "ngit init --force exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    let republished = fetch_republished_announcement(
        &harness,
        state.keys.public_key(),
        &state.coordinate_identifier,
        state.existing_announcement.created_at,
    )
    .await?;

    let additional_maintainer_pubkey = state
        .additional_maintainer_keys
        .first()
        .context("State C arrange should mint at least one additional maintainer")?
        .public_key();

    Ok(ForceRefreshSnapshot {
        republished,
        existing_name: state.existing_name,
        existing_description: state.existing_description,
        marker_relay_url: state.marker_relay_url,
        maintainer_pubkey: state.keys.public_key(),
        additional_maintainer_pubkey,
    })
}

/// Equivalent of legacy
/// `state_c_my_announcement::success::force_refresh::name_preserved`.
/// `--force` republishes with the existing `name` tag unchanged.
#[rstest]
#[tokio::test]
async fn force_refresh_preserves_name(
    #[future] force_refresh_snapshot: Arc<ForceRefreshSnapshot>,
) -> Result<()> {
    let s = force_refresh_snapshot.await;
    assert_eq!(
        tag_value(&s.republished, "name").as_deref(),
        Some(s.existing_name.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_c_my_announcement::success::force_refresh::description_preserved`.
#[rstest]
#[tokio::test]
async fn force_refresh_preserves_description(
    #[future] force_refresh_snapshot: Arc<ForceRefreshSnapshot>,
) -> Result<()> {
    let s = force_refresh_snapshot.await;
    assert_eq!(
        tag_value(&s.republished, "description").as_deref(),
        Some(s.existing_description.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_c_my_announcement::success::force_refresh::relays_from_my_event`.
/// The marker URL — present in the existing announcement's `relays`
/// tag but nowhere in ngit's own default relay-set — survives into
/// the republished announcement's `relays` tag.
#[rstest]
#[tokio::test]
async fn force_refresh_preserves_marker_relay(
    #[future] force_refresh_snapshot: Arc<ForceRefreshSnapshot>,
) -> Result<()> {
    let s = force_refresh_snapshot.await;
    let relays = tag_values(&s.republished, "relays");
    assert!(
        relays.iter().any(|r| r == &s.marker_relay_url),
        "republished announcement's `relays` tag should preserve the \
         marker URL ({}); got {relays:?}",
        s.marker_relay_url,
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_c_my_announcement::success::force_refresh::maintainers_preserved`.
/// Both the publisher and the arrange-minted co-maintainer survive into
/// the republished announcement's `maintainers` tag.
#[rstest]
#[tokio::test]
async fn force_refresh_preserves_maintainers(
    #[future] force_refresh_snapshot: Arc<ForceRefreshSnapshot>,
) -> Result<()> {
    let s = force_refresh_snapshot.await;
    let maintainers = tag_values(&s.republished, "maintainers");
    let maintainer_hex = s.maintainer_pubkey.to_string();
    let additional_hex = s.additional_maintainer_pubkey.to_string();
    assert!(
        maintainers.contains(&maintainer_hex),
        "republished announcement's `maintainers` tag should include the \
         publisher ({maintainer_hex}); got {maintainers:?}",
    );
    assert!(
        maintainers.contains(&additional_hex),
        "republished announcement's `maintainers` tag should include the \
         arrange-minted co-maintainer ({additional_hex}); got {maintainers:?}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Success — `ngit init --name "New Name"` (name-override fixture)
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --name "New Name"` invocation
/// against a State C repo. Two assertion cases share this snapshot.
struct NameOverrideSnapshot {
    republished: Event,
    /// What the arrange set the coordinate identifier to. Asserted-equal
    /// to the republished announcement's `d` tag — `--name` does not
    /// re-derive the identifier (legacy `identifier_unchanged`).
    coordinate_identifier: String,
}

const NAME_OVERRIDE_NEW_NAME: &str = "New Name";

static NAME_OVERRIDE_SNAPSHOT: OnceCell<Arc<NameOverrideSnapshot>> = OnceCell::const_new();

#[fixture]
async fn name_override_snapshot() -> Arc<NameOverrideSnapshot> {
    NAME_OVERRIDE_SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_name_override()
                    .await
                    .expect("init_state_my_announcement: capture_name_override failed"),
            )
        })
        .await
        .clone()
}

async fn capture_name_override() -> Result<NameOverrideSnapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_c_my_announcement().await?;

    let init_out = repo
        .ngit(["init", "--name", NAME_OVERRIDE_NEW_NAME])
        .output()
        .await
        .context("failed to spawn ngit init --name \"New Name\"")?;
    if !init_out.status.success() {
        bail!(
            "ngit init --name exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    let republished = fetch_republished_announcement(
        &harness,
        state.keys.public_key(),
        &state.coordinate_identifier,
        state.existing_announcement.created_at,
    )
    .await?;

    Ok(NameOverrideSnapshot {
        republished,
        coordinate_identifier: state.coordinate_identifier,
    })
}

/// Equivalent of legacy
/// `state_c_my_announcement::success::name_override::name_overridden`.
/// `--name "New Name"` replaces the `name` tag.
#[rstest]
#[tokio::test]
async fn name_override_replaces_name(
    #[future] name_override_snapshot: Arc<NameOverrideSnapshot>,
) -> Result<()> {
    let s = name_override_snapshot.await;
    assert_eq!(
        tag_value(&s.republished, "name").as_deref(),
        Some(NAME_OVERRIDE_NEW_NAME),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_c_my_announcement::success::name_override::identifier_unchanged`.
/// `--name` does not re-derive the identifier; the `d` tag still equals
/// the existing coordinate identifier.
#[rstest]
#[tokio::test]
async fn name_override_keeps_identifier(
    #[future] name_override_snapshot: Arc<NameOverrideSnapshot>,
) -> Result<()> {
    let s = name_override_snapshot.await;
    assert_eq!(
        tag_value(&s.republished, "d").as_deref(),
        Some(s.coordinate_identifier.as_str()),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query the default relay for the kind-30617 with matching
/// `(pubkey, d)` whose `created_at` strictly exceeds `not_after` — i.e.
/// the *republished* event, not the back-dated existing one the
/// arrange already put on the relay.
///
/// Replaceable-event semantics mean the relay should only carry one
/// kind-30617 per `(pubkey, d)` triple (the newer one), but querying
/// by `created_at > existing.created_at` is the safer assertion: if a
/// regression ever caused ngit to silently *not* republish, this
/// helper would error out with "no republished announcement found"
/// instead of returning the stale event and a downstream assertion
/// passing spuriously against unchanged tags.
async fn fetch_republished_announcement(
    harness: &Harness,
    author: PublicKey,
    identifier: &str,
    not_after: Timestamp,
) -> Result<Event> {
    let announcements = harness
        .relay("default")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?;
    announcements
        .into_iter()
        .filter(|e| tag_value(e, "d").as_deref() == Some(identifier))
        .filter(|e| e.created_at > not_after)
        .max_by_key(|e| e.created_at)
        .with_context(|| {
            format!(
                "no republished kind-30617 with `d` = {identifier:?} and \
                 created_at > {not_after} on the default relay after \
                 `ngit init` — did the republish fail silently?"
            )
        })
}

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
