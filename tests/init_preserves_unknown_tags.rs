//! `ngit init` round-trips unknown tags on the existing repo
//! announcement instead of silently stripping them.
//!
//! Motivating problem: `RepoRef::try_from` parses only the tags ngit
//! itself knows about (`d`, `name`, `description`, `clone`, `web`,
//! `r`/`euc`, `relays`, `t`, `blossoms`, `maintainers`) and
//! `RepoRef::to_event` rebuilds the tag list from the typed struct
//! fields. Anything else on the source event is dropped on every
//! republish. If a future ngit version or a third-party tool adds a
//! new tag, today's `ngit init` silently erases it the next time the
//! maintainer runs it.
//!
//! The contract these tests pin:
//!
//! - **Preserve by default.** `ngit init --force` against a State C
//!   announcement carrying unknown tags republishes them verbatim, including
//!   repeated tags of the same unknown name (`multi-example- style-2` shape).
//! - **Multi-value tags survive intact.** A single unknown tag with multiple
//!   value slots (`["multi-example", "v1", "v2"]`) round- trips as one tag with
//!   both values, not as two tags or one truncated tag.
//! - **Known-name duplicates collapse.** If the source event carries two tags
//!   whose first slot is a *known* name (e.g. two `name` tags), the republished
//!   event carries exactly one — ngit's typed field is the single source of
//!   truth for known names. The extras are not smuggled back in via the
//!   unknown-tag pass-through.
//! - **`--clean` drops them.** `ngit init --force --clean` against the same
//!   arrange republishes with the unknown tags removed.
//!
//! ## Coverage
//!
//! Two captured snapshots, each shared across multiple `#[rstest]`
//! cases via `tokio::sync::OnceCell`:
//!
//! - `PreserveSnapshot` — `ngit init --force`
//!   - `preserve_keeps_single_example`
//!   - `preserve_keeps_multi_value`
//!   - `preserve_keeps_repeated_style_2`
//!   - `preserve_dedupes_name_tag`
//! - `CleanSnapshot` — `ngit init --force --clean`
//!   - `clean_drops_example`
//!   - `clean_drops_multi_example`
//!   - `clean_drops_multi_example_style_2`
//!
//! ## Why two snapshots
//!
//! `--force` and `--force --clean` exercise the same MyAnnouncement
//! republish path with the same arrange but different ngit CLI flags;
//! one snapshot can't observe both behaviours. Two `OnceCell` fixtures
//! match the `init_state_my_announcement.rs` precedent.
//!
//! ## Why State C
//!
//! State C ("MyAnnouncement") is the simplest republish path: a single
//! existing announcement signed by the publisher, no co-maintainer
//! cascade, no grasp-server choreography in scope. Tag pass-through
//! is a `RepoRef`-level concern, so the cheapest arrange that exercises
//! the parse-then-emit round-trip is sufficient. Inheritance from a
//! *co-maintainer's* announcement (State D) is a separate concern not
//! covered here.
//!
//! ## Assertions are tag-shape only
//!
//! Per harness rules, no exact-stdout assertions. The yellow warning
//! ngit prints when preserving unknown tags is verified by its
//! observable side-effect (tags present in the republished event), not
//! by reading stdout/stderr.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::Harness;
use tokio::sync::OnceCell;

// ---------------------------------------------------------------------------
// Fixture tags
// ---------------------------------------------------------------------------

/// Unknown tags appended to the fabricated State C announcement.
/// Shape is the literal list in the user's spec for this feature so
/// the test breakage signal points straight back at the requirement.
fn fixture_extra_tags() -> Vec<Tag> {
    vec![
        Tag::parse(["example", "value"]).unwrap(),
        Tag::parse(["multi-example", "value1", "value2"]).unwrap(),
        Tag::parse(["multi-example-style-2", "value1"]).unwrap(),
        Tag::parse(["multi-example-style-2", "value2"]).unwrap(),
        // Repeated *known* tag names. ngit's typed `name` field is the
        // single source of truth: the republished event should carry
        // exactly one `name` tag, not three (one from the typed field,
        // two passed-through). Asserted by `preserve_dedupes_name_tag`.
        Tag::parse(["name", "name1"]).unwrap(),
        Tag::parse(["name", "name2"]).unwrap(),
    ]
}

// ---------------------------------------------------------------------------
// Snapshot — `ngit init --force` (preserve fixture)
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --force` invocation against
/// a State C repo whose existing announcement carries
/// [`fixture_extra_tags`]. Read-only; shared across `#[rstest]` cases
/// via `OnceCell`.
struct PreserveSnapshot {
    /// The kind-30617 event ngit republished. Strictly newer than the
    /// arrange's `existing_announcement` (which is back-dated 30s by
    /// `arrange_init_state_c_my_announcement_with_extra_tags`), so the
    /// helper that fetches it can disambiguate by `created_at`.
    republished: Event,
}

static PRESERVE_SNAPSHOT: OnceCell<Arc<PreserveSnapshot>> = OnceCell::const_new();

#[fixture]
async fn preserve_snapshot() -> Arc<PreserveSnapshot> {
    PRESERVE_SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_preserve()
                    .await
                    .expect("init_preserves_unknown_tags: capture_preserve failed"),
            )
        })
        .await
        .clone()
}

async fn capture_preserve() -> Result<PreserveSnapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, state) = harness
        .arrange_init_state_c_my_announcement_with_extra_tags(fixture_extra_tags())
        .await?;

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

    Ok(PreserveSnapshot { republished })
}

/// Without `--clean`, the single-value unknown tag `["example", "value"]`
/// is republished verbatim — exactly one tag whose first slot is
/// `"example"` and whose value slot is `"value"`.
#[rstest]
#[tokio::test]
async fn preserve_keeps_single_example(
    #[future] preserve_snapshot: Arc<PreserveSnapshot>,
) -> Result<()> {
    let s = preserve_snapshot.await;
    let matching: Vec<&[String]> = tags_with_name(&s.republished, "example");
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one `example` tag on the republished announcement; got {}: {matching:?}",
        matching.len(),
    );
    assert_eq!(
        matching[0],
        &["example".to_string(), "value".to_string()][..],
        "`example` tag should round-trip verbatim",
    );
    Ok(())
}

/// A multi-value unknown tag (`["multi-example", "v1", "v2"]`) round-
/// trips as one tag with both values, not split into two tags and not
/// truncated to one value.
#[rstest]
#[tokio::test]
async fn preserve_keeps_multi_value(
    #[future] preserve_snapshot: Arc<PreserveSnapshot>,
) -> Result<()> {
    let s = preserve_snapshot.await;
    let matching: Vec<&[String]> = tags_with_name(&s.republished, "multi-example");
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one `multi-example` tag on the republished announcement; got {}: {matching:?}",
        matching.len(),
    );
    assert_eq!(
        matching[0],
        &[
            "multi-example".to_string(),
            "value1".to_string(),
            "value2".to_string()
        ][..],
        "`multi-example` tag should round-trip verbatim with both values",
    );
    Ok(())
}

/// Two separate tags with the same unknown name
/// (`multi-example-style-2`, one value each) survive as two distinct
/// tags. Required by any schema that legitimately uses repeated tags
/// of the same name (NIP-style `t`/`r`/etc. shape for unknown
/// namespaces).
#[rstest]
#[tokio::test]
async fn preserve_keeps_repeated_style_2(
    #[future] preserve_snapshot: Arc<PreserveSnapshot>,
) -> Result<()> {
    let s = preserve_snapshot.await;
    let matching: Vec<&[String]> = tags_with_name(&s.republished, "multi-example-style-2");
    assert_eq!(
        matching.len(),
        2,
        "expected exactly two `multi-example-style-2` tags on the \
         republished announcement; got {}: {matching:?}",
        matching.len(),
    );
    let values: Vec<&str> = matching
        .iter()
        .filter_map(|t| t.get(1).map(String::as_str))
        .collect();
    assert!(
        values.contains(&"value1") && values.contains(&"value2"),
        "both `value1` and `value2` should be preserved across the two \
         `multi-example-style-2` tags; got values={values:?}",
    );
    Ok(())
}

/// Two repeated *known*-name tags (`["name", "name1"]` /
/// `["name", "name2"]`) on the source event collapse to exactly one
/// `name` tag on republish — ngit's typed `name` field is the single
/// source of truth, the duplicates are not smuggled back in via the
/// unknown-tag pass-through.
///
/// The value of the surviving `name` tag is whatever ngit's typed
/// `name` field resolved to (the existing announcement's original
/// `name` value); the contract we pin here is "exactly one `name`
/// tag", not which winner.
#[rstest]
#[tokio::test]
async fn preserve_dedupes_name_tag(
    #[future] preserve_snapshot: Arc<PreserveSnapshot>,
) -> Result<()> {
    let s = preserve_snapshot.await;
    let matching: Vec<&[String]> = tags_with_name(&s.republished, "name");
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one `name` tag on the republished announcement \
         (known-name duplicates from the source event must collapse); \
         got {}: {matching:?}",
        matching.len(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot — `ngit init --force --clean` (clean fixture)
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --force --clean` invocation
/// against the same State C arrange as [`PreserveSnapshot`].
struct CleanSnapshot {
    republished: Event,
}

static CLEAN_SNAPSHOT: OnceCell<Arc<CleanSnapshot>> = OnceCell::const_new();

#[fixture]
async fn clean_snapshot() -> Arc<CleanSnapshot> {
    CLEAN_SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_clean()
                    .await
                    .expect("init_preserves_unknown_tags: capture_clean failed"),
            )
        })
        .await
        .clone()
}

async fn capture_clean() -> Result<CleanSnapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, state) = harness
        .arrange_init_state_c_my_announcement_with_extra_tags(fixture_extra_tags())
        .await?;

    let init_out = repo
        .ngit(["init", "--force", "--clean"])
        .output()
        .await
        .context("failed to spawn ngit init --force --clean")?;
    if !init_out.status.success() {
        bail!(
            "ngit init --force --clean exited non-zero ({:?})\nstdout: {}\nstderr: {}",
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

    Ok(CleanSnapshot { republished })
}

/// `--clean` strips the `example` unknown tag.
#[rstest]
#[tokio::test]
async fn clean_drops_example(#[future] clean_snapshot: Arc<CleanSnapshot>) -> Result<()> {
    let s = clean_snapshot.await;
    let matching = tags_with_name(&s.republished, "example");
    assert!(
        matching.is_empty(),
        "expected no `example` tag on `--clean` republish; got {matching:?}",
    );
    Ok(())
}

/// `--clean` strips the multi-value `multi-example` unknown tag.
#[rstest]
#[tokio::test]
async fn clean_drops_multi_example(#[future] clean_snapshot: Arc<CleanSnapshot>) -> Result<()> {
    let s = clean_snapshot.await;
    let matching = tags_with_name(&s.republished, "multi-example");
    assert!(
        matching.is_empty(),
        "expected no `multi-example` tag on `--clean` republish; got {matching:?}",
    );
    Ok(())
}

/// `--clean` strips *both* of the repeated `multi-example-style-2`
/// unknown tags.
#[rstest]
#[tokio::test]
async fn clean_drops_multi_example_style_2(
    #[future] clean_snapshot: Arc<CleanSnapshot>,
) -> Result<()> {
    let s = clean_snapshot.await;
    let matching = tags_with_name(&s.republished, "multi-example-style-2");
    assert!(
        matching.is_empty(),
        "expected no `multi-example-style-2` tag on `--clean` republish; got {matching:?}",
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
/// Same shape as the helper in `tests/init_state_my_announcement.rs`;
/// duplicated here rather than shared because tests/* binaries can't
/// share helper modules without a `mod common` shim and the duplication
/// is a few lines.
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
        .filter(|e| tag_first_value(e, "d").as_deref() == Some(identifier))
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
fn tag_first_value(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

/// All tags (as raw `&[String]` slices) whose first slot equals `key`.
/// Returning the whole slice (not just slot 1) lets callers assert on
/// multi-value tags (`["multi-example", "v1", "v2"]`) and on tag count
/// in one pass.
fn tags_with_name<'a>(event: &'a Event, key: &str) -> Vec<&'a [String]> {
    event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|s| s.first().map(String::as_str) == Some(key))
        .collect()
}
