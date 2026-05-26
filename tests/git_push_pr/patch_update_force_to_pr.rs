//! Coverage for `git push --force` on top of an existing **patch-kind**
//! proposal where the amended tip exceeds the per-patch size threshold,
//! triggering the patch→PR upgrade path.
//!
//! ## Scenario
//!
//! Mirrors [`super::patch_update_force`] up to and including the amend step,
//! but the amended file content is large enough (~100 KiB) that the resulting
//! patch for that commit exceeds the 64 KiB recommendation enforced by
//! [`Repo::are_commits_too_big_for_patches`] (`src/lib/git/mod.rs:448-459`).
//!
//! That triggers the size-driven branch in
//! `git_remote_nostr/push.rs:533-552`:
//!
//! ```text
//! if effective_root.kind.eq(&KIND_PULL_REQUEST)
//!     || git_repo.are_commits_too_big_for_patches(&ahead)
//!     || git_repo.do_commits_contain_submodules(&ahead)
//! {
//!     // emit a PR-kind event
//! }
//! ```
//!
//! Because the existing thread root is patch-kind, the upgrade path runs
//! `generate_unsigned_pr_or_update_event` with `root_proposal =
//! Some(original_root_patch)` and a non-empty `root_patch_cover_letter`
//! (`git_events.rs:483-491`).  The resulting event is `KIND_PULL_REQUEST`
//! (not `KIND_PULL_REQUEST_UPDATE`) and carries the
//! `pr_specific_tags()`-with-cover-letter shape from
//! `git_events.rs:545-556`:
//!
//! * `["e", <original_root_patch_id>]` — 2-slot reference to the original patch
//!   root (no marker; this is the cover-letter back-link, distinct from the
//!   4-slot threading markers carried by `Kind::GitPatch`).
//! * `["branch-name", <branch>]` — derived from the root patch's `branch-name`
//!   tag via `event_to_cover_letter`.
//! * `["p", <original_root_pubkey>]` — back-reference to the contributor who
//!   authored the original patch series.
//!
//! ## Assertions (one `#[rstest]` per)
//!
//! - `three_patch_events_total` — 2 original + 1 FF push, no new patches from
//!   the force push (the upgrade replaces patches with a single PR)
//! - `one_pr_event` — exactly one `KIND_PULL_REQUEST` event on the GRASP
//! - `zero_pr_update_events` — patch→PR upgrade emits `KIND_PULL_REQUEST`, not
//!   `KIND_PULL_REQUEST_UPDATE`
//! - `pr_event_e_tag_references_original_root` — PR event carries an `e` tag
//!   with the original root patch id in slot 1
//! - `pr_event_p_tag_references_original_root_author` — PR event carries a `p`
//!   tag with the original root patch author's pubkey
//! - `pr_event_branch_name_matches_original_series` — PR event's `branch-name`
//!   tag equals the original series branch name
//! - `pr_event_c_tag_is_amended_tip` — PR event's `c` tag equals the amended
//!   tip OID
//! - `pr_event_authored_by_maintainer` — PR event is signed by the maintainer
//!   (the actor who ran the force push)

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishPatchSeriesOpts,
    PublishRepoOpts, event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for this test repo — distinct from every other `git_push_pr`
/// scenario so the shared vanilla relay's REQ surface stays uncontaminated.
const IDENTIFIER: &str = "git-push-pr-patch-update-force-to-pr";

/// Size of the amended file's body, in bytes.  Must exceed the per-patch
/// threshold of `(65 - 1) * 1024 = 65536` bytes enforced by
/// `Repo::are_commits_too_big_for_patches` (`src/lib/git/mod.rs:448-459`).
/// 100 KiB gives comfortable headroom over the 64 KiB limit even after
/// accounting for the small patch header overhead.
const BIG_FILE_BYTES: usize = 100 * 1024;

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Everything observable after the publish → first-push → big-amend →
/// force-push arrangement, captured once per test binary via [`SNAPSHOT`]
/// and shared read-only across all `#[rstest]` cases.
struct Snapshot {
    /// All `Kind::GitPatch` events on the GRASP after the force push.
    /// Expected count: 3 (2 original + 1 first push).  The force push
    /// emits a PR event, not new patches.
    all_patch_events: Vec<Event>,

    /// All `KIND_PULL_REQUEST` events on the GRASP after the force push.
    /// Expected: exactly one — the patch→PR upgrade event.
    pr_events: Vec<Event>,

    /// The single upgrade PR event extracted from [`Self::pr_events`] —
    /// disambiguated by `branch-name` matching the original series.
    pr_event: Event,

    /// Count of `KIND_PULL_REQUEST_UPDATE` events on the GRASP.  Must be
    /// 0 — the upgrade is itself the new root, not an update.
    pr_update_count: usize,

    /// Event ID of the original series root patch published by
    /// [`Harness::publish_patch_series`].  The upgrade PR event's `e` tag
    /// must equal this.
    original_root_patch_id: EventId,

    /// Pubkey of the original series root patch (the contributor).  The
    /// upgrade PR event's `p` tag must equal this.
    original_root_patch_pubkey: PublicKey,

    /// Branch name of the original series — what the upgrade PR event's
    /// `branch-name` tag must equal (after `safe_branch_name_for_pr`
    /// normalisation, which is a no-op for alphanumeric+dash names like
    /// the harness default `feature-1`).
    original_branch_name: String,

    /// Amended tip OID (post-amend `HEAD`) — what the upgrade PR event's
    /// `c` tag must equal.
    amended_tip_oid: String,

    /// Maintainer's pubkey — the actor running the force push and
    /// therefore the expected signer of the upgrade PR event.
    maintainer_pubkey: PublicKey,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary via
/// [`SNAPSHOT`] and hand each case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot().await.expect(
                    "git_push_pr::patch_update_force_to_pr fixture: capture_snapshot failed",
                ),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- 1. Harness ----------------------------------------------------------
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    // --- 2. Maintainer publishes the repo ------------------------------------
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("patch-update-force-to-pr maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    let maintainer_keys = Keys::parse(&published.maintainer_nsec)
        .context("published.maintainer_nsec is not a valid key")?;
    let maintainer_pubkey = maintainer_keys.public_key();

    // --- 3. Contributor publishes a patch-series proposal --------------------
    //
    // Defaults: branch = "feature-1", two commits (t3.md, t4.md), no cover
    // letter.  `--force-patch` is set inside the helper so the kind doesn't
    // depend on the default-kind heuristic in `ngit send`.
    let series = harness
        .publish_patch_series(&published, PublishPatchSeriesOpts::default())
        .await?;

    // --- 4. Identify the original series root --------------------------------
    //
    // The root is the first patch in the series — the only one whose
    // `branch-name` tag is set.  Its event ID is the back-reference the
    // upgrade PR event must carry in its `e` tag.
    let original_root_patch = series
        .patch_events
        .iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(series.branch_name.as_str()))
        .with_context(|| {
            format!(
                "no patch event with branch-name={:?} in publish_patch_series output; \
                 has the harness changed how it tags the series root?",
                series.branch_name,
            )
        })?;
    let original_root_patch_id = original_root_patch.id;
    let original_root_patch_pubkey = original_root_patch.pubkey;
    let shorthand = &original_root_patch.id.to_hex()[..8];
    let remote_branch = format!("pr/{}({})", series.branch_name, shorthand);

    // --- 5. Maintainer clones ------------------------------------------------
    let maintainer_clone = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    // Verify the patch branch is advertised before attempting checkout.
    let snap = maintainer_clone
        .snapshot()
        .context("snapshotting maintainer clone before checkout")?;
    let remote_ref = format!("refs/remotes/origin/{remote_branch}");
    snap.refs.get(&remote_ref).with_context(|| {
        format!(
            "{remote_ref} missing from maintainer clone after `git clone` — \
             list.rs no longer advertises patch proposals as pr/<branch>(<shorthand>) \
             for non-author viewers?  Available refs: {:?}",
            snap.refs.keys().collect::<Vec<_>>(),
        )
    })?;

    // --- 6. Checkout proposal branch and commit ------------------------------
    maintainer_clone
        .git_ok(
            ["checkout", &remote_branch],
            &format!("git checkout {remote_branch}"),
        )
        .await?;

    // The first follow-up content is intentionally small — this push must
    // land as a `Kind::GitPatch`, not a PR.  Only the *amended* version of
    // this file (step 8) is supersized to trigger the upgrade.
    std::fs::write(
        maintainer_clone.dir().join("maintainer-update.md"),
        "maintainer follow-up\n",
    )
    .context("failed to write maintainer-update.md")?;
    maintainer_clone
        .git_ok(
            ["add", "maintainer-update.md"],
            "git add maintainer-update.md",
        )
        .await?;
    maintainer_clone
        .git_ok(
            ["commit", "-m", "follow-up on patch series", "--no-gpg-sign"],
            "git commit maintainer-update.md",
        )
        .await?;

    // --- 7. First push (fast-forward, patch-kind) ----------------------------
    //
    // `nostr_push` ticks one unix second before pushing.  Adds one
    // `Kind::GitPatch` event covering the new commit.  Total patch events: 3.
    maintainer_clone
        .nostr_push(["origin", &remote_branch])
        .await
        .context("nostr_push of maintainer follow-up (first push) failed")?;

    // --- 8. Amend the tip commit with >64 KiB content ------------------------
    //
    // `are_commits_too_big_for_patches` (src/lib/git/mod.rs:448-459) measures
    // the size of `make_patch_from_commit` output.  A textual diff that adds
    // a 100 KiB file (replacing the prior small content) produces a patch
    // well above the 64 KiB threshold, so the force-push code path picks
    // PR-kind via `git_remote_nostr/push.rs:533-552`.
    //
    // Use a deterministic, easily-compressible string so the file's content
    // is bounded in size and obvious in any diagnostic dump.
    let big_content: String = "x".repeat(BIG_FILE_BYTES);
    std::fs::write(
        maintainer_clone.dir().join("maintainer-update.md"),
        &big_content,
    )
    .context("failed to write supersized maintainer-update.md")?;
    maintainer_clone
        .git_ok(
            ["add", "maintainer-update.md"],
            "git add maintainer-update.md (amend step)",
        )
        .await?;
    maintainer_clone
        .git_ok(
            [
                "commit",
                "--amend",
                "-m",
                "amended follow-up on patch series (big)",
                "--no-gpg-sign",
            ],
            "git commit --amend",
        )
        .await?;

    let amended_tip_oid = maintainer_clone
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after amend")?;

    // --- 9. Force push (patch → PR upgrade) ----------------------------------
    //
    // `nostr_push` ticks another unix second before pushing.  The `-f`
    // refspec prefix is detected by `push.rs:485` as a force push.  The
    // remote helper resolves all commits ahead of `main` (3 commits: the
    // two original contributor commits plus the amended-and-supersized
    // maintainer commit) and `are_commits_too_big_for_patches` returns
    // true because the amended commit's patch exceeds 64 KiB.  This
    // routes through the PR-emission arm: one `KIND_PULL_REQUEST` event,
    // no new `Kind::GitPatch` events.
    maintainer_clone
        .nostr_push(["-f", "origin", &remote_branch])
        .await
        .context("nostr_push -f (force push after big-content amend) failed")?;

    // --- 10. Query GRASP state -----------------------------------------------
    let all_patch_events = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitPatch))
        .await?;

    let pr_events = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST))
        .await?;

    let pr_update_count = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST_UPDATE))
        .await?
        .len();

    // --- 11. Identify the upgrade PR event -----------------------------------
    //
    // The PR event derives its `branch-name` tag from the original root
    // patch's cover-letter (via `event_to_cover_letter`).  Match on that to
    // disambiguate from any stray PR-kind events that future scenarios might
    // accidentally publish.
    let pr_event = pr_events
        .iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(series.branch_name.as_str()))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no KIND_PULL_REQUEST event on GRASP with branch-name={:?} after the \
                 force push; all PR event ids: {:?}",
                series.branch_name,
                pr_events.iter().map(|e| e.id.to_hex()).collect::<Vec<_>>(),
            )
        })?;

    Ok(Snapshot {
        all_patch_events,
        pr_events,
        pr_event,
        pr_update_count,
        original_root_patch_id,
        original_root_patch_pubkey,
        original_branch_name: series.branch_name.clone(),
        amended_tip_oid,
        maintainer_pubkey,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Exactly 3 `Kind::GitPatch` events on the GRASP: 2 from
/// `publish_patch_series` + 1 from the first (FF) push.
///
/// A count of 6 would mean the force push fell through to the patch
/// revision path and emitted 3 new patches (the
/// [`super::patch_update_force`] behaviour) — failing the upgrade.  A count
/// > 3 but < 6 would mean partial emission and probably indicates a logic
/// bug producing both kinds.
#[rstest]
#[tokio::test]
async fn three_patch_events_total(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.all_patch_events.len(),
        3,
        "expected exactly 3 Kind::GitPatch events on GRASP \
         (2 original + 1 first push; force push must emit a PR not patches); \
         got {} (event ids: {:?})",
        s.all_patch_events.len(),
        s.all_patch_events
            .iter()
            .map(|e| e.id.to_hex())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Exactly one `KIND_PULL_REQUEST` event on the GRASP — the patch→PR
/// upgrade that the size threshold triggered.
#[rstest]
#[tokio::test]
async fn one_pr_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_events.len(),
        1,
        "expected exactly one KIND_PULL_REQUEST event on GRASP after the \
         size-triggered force push; got {} (event ids: {:?})",
        s.pr_events.len(),
        s.pr_events
            .iter()
            .map(|e| e.id.to_hex())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Zero `KIND_PULL_REQUEST_UPDATE` events — patch→PR upgrade emits a
/// fresh `KIND_PULL_REQUEST` (the root of the new PR thread), not an
/// update.  An update would only be valid if the thread root were
/// already PR-kind.
#[rstest]
#[tokio::test]
async fn zero_pr_update_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events on GRASP; got {}",
        s.pr_update_count,
    );
    Ok(())
}

/// Upgrade PR event carries an `e` tag pointing at the original series
/// root patch — this is the back-reference produced by
/// `pr_specific_tags()` (`git_events.rs:545-556`) when the upgrade path
/// finds an existing patch-kind root.
///
/// Slot 1 of the first `e` tag must equal `original_root_patch_id`.
/// Matches the 2-slot shape `["e", <hex>]`; we do not enforce the slot
/// count because future revisions may extend the tag.
#[rstest]
#[tokio::test]
async fn pr_event_e_tag_references_original_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let e_values: Vec<String> = s
        .pr_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("e"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();
    assert!(
        e_values.contains(&s.original_root_patch_id.to_hex()),
        "PR event should carry an `e` tag with the original root patch id {:?}; \
         got e values: {:?}",
        s.original_root_patch_id.to_hex(),
        e_values,
    );
    Ok(())
}

/// Upgrade PR event carries a `p` tag with the original root patch's
/// author pubkey — the contributor.  Produced by
/// `Tag::public_key(root_proposal.unwrap().pubkey)` in
/// `pr_specific_tags()` (`git_events.rs:555`).
#[rstest]
#[tokio::test]
async fn pr_event_p_tag_references_original_root_author(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let p_values: Vec<String> = s
        .pr_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("p"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();
    assert!(
        p_values.contains(&s.original_root_patch_pubkey.to_hex()),
        "PR event should carry a `p` tag with the original root patch author \
         {:?}; got p values: {:?}",
        s.original_root_patch_pubkey.to_hex(),
        p_values,
    );
    Ok(())
}

/// Upgrade PR event's `branch-name` tag equals the original series'
/// branch name.
///
/// `event_to_cover_letter` reads `branch-name` off the root patch and
/// passes it through `safe_branch_name_for_pr` — a no-op for
/// alphanumeric+dash names like the harness default `feature-1`.
#[rstest]
#[tokio::test]
async fn pr_event_branch_name_matches_original_series(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "branch-name").as_deref(),
        Some(s.original_branch_name.as_str()),
        "PR event branch-name tag should be {:?} (carried over from the \
         original root patch's cover letter); got {:?}",
        s.original_branch_name,
        tag_value(&s.pr_event, "branch-name"),
    );
    Ok(())
}

/// Upgrade PR event's `c` tag equals the amended tip OID — what the
/// force push actually published.
#[rstest]
#[tokio::test]
async fn pr_event_c_tag_is_amended_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "c").as_deref(),
        Some(s.amended_tip_oid.as_str()),
        "PR event c tag should equal amended tip OID {:?}; got {:?}",
        s.amended_tip_oid,
        tag_value(&s.pr_event, "c"),
    );
    Ok(())
}

/// Upgrade PR event is signed by the maintainer — the actor running the
/// force push owns the upgrade, not the original series author.
#[rstest]
#[tokio::test]
async fn pr_event_authored_by_maintainer(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_event.pubkey,
        s.maintainer_pubkey,
        "upgrade PR event should be authored by the maintainer ({}) who ran \
         the force push; got {}",
        s.maintainer_pubkey.to_hex(),
        s.pr_event.pubkey.to_hex(),
    );
    Ok(())
}
