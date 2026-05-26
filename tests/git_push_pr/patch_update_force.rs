//! Coverage for `git push --force` on top of an existing **patch-kind**
//! proposal after amending the tip commit — including a *second* force push
//! that produces a revision of a revision.
//!
//! ## Scenario
//!
//! Extends [`super::patch_update`]'s fast-forward push with two
//! amend-and-force-push steps.  A contributor publishes a two-commit patch
//! series via `ngit send --force-patch`; the maintainer then clones, checks
//! out the proposal branch, adds a commit as a fast-forward push, amends and
//! force-pushes (revision #1), then amends again and force-pushes a second
//! time (revision #2).
//!
//! Each force push triggers a **revision** of the patch series:
//! `generate_cover_letter_and_patch_events` is called (via
//! `generate_patches_or_pr_event_or_pr_updates`) with all commits ahead of
//! `main` and with `root_proposal = Some(original_root_patch)` as the
//! revision anchor.  Each revision produces three new `Kind::GitPatch`
//! events.
//!
//! ## Revision structure (no cover letter)
//!
//! With no cover letter the first commit's patch acts as the revision root.
//! For the 3-commit series [c1, c2, cN] re-published by a force push:
//!
//! * **c1 patch** (revision root) — carries:
//!   - `["t", "root"]`
//!   - `["t", "root-revision"]` (alias `"revision-root"` also accepted)
//!   - `["e", <original_root_patch_id>, _, "reply"]`
//! * **c2 patch** — carries:
//!   - `["e", <c1_patch_id>, _, "root"]`
//!   - `["e", <c1_patch_id>, _, "reply"]` (c1 is both thread root and direct
//!     parent)
//! * **cN patch** (amended tip) — carries:
//!   - `["e", <c1_patch_id>, _, "root"]`
//!   - `["e", <c2_patch_id>, _, "reply"]`
//!
//! ## Where does the *second* revision's `e reply` point?
//!
//! On every force push the remote helper calls
//! `find_proposal_and_patches_by_branch_name` (lib/utils.rs:271), which
//! searches the map returned by `get_all_proposals`.  That map *filters out*
//! events for which `event_is_revision_root` is true (lib/utils.rs:208), so
//! it can only ever return the **original** proposal root.  That event
//! becomes `effective_root` and is passed all the way down as
//! `root_proposal_id` to `generate_patch_event` (lib/git_events.rs:252),
//! which writes its id into the new revision root's `e reply` tag.
//!
//! Therefore the second revision root should reply to the *original* root
//! patch, **not** to the first revision root.  `revision_2_root_replies_to_
//! original_root` asserts this empirically.
//!
//! ## Assertions (one `#[rstest]` per)
//!
//! - `nine_patch_events_total` — 2 original + 1 first push + 3 revision #1 + 3
//!   revision #2 = 9
//! - `zero_pr_events` — no `KIND_PULL_REQUEST` emitted
//! - `zero_pr_update_events` — no `KIND_PULL_REQUEST_UPDATE` emitted
//! - `revision_root_has_t_root` — revision #1 root carries `["t", "root"]`
//! - `revision_root_has_t_revision_root` — carries `["t", "root-revision"]`
//! - `revision_root_replies_to_original_root` — `["e", <original_id>, _,
//!   "reply"]`
//! - `tip_patch_root_is_revision_root` — `["e", <revision_root_id>, _, "root"]`
//! - `tip_patch_replies_to_second_patch` — `["e", <revision_patch_2_id>, _,
//!   "reply"]`
//! - `revision_2_root_replies_to_original_root` — the **second** revision
//!   root's `e reply` points at the original root patch, *not* the first
//!   revision root
//! - `revision_2_tip_root_is_revision_2_root` — the second revision tip threads
//!   under the second revision root

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishPatchSeriesOpts,
    PublishRepoOpts, event_branch_name_tag, tag_value, tag_values_multiple,
};
use tokio::sync::OnceCell;

/// Identifier for this test repo — distinct from every other `git_push_pr`
/// scenario so the shared vanilla relay's REQ surface stays uncontaminated.
const IDENTIFIER: &str = "git-push-pr-patch-update-force";

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Everything observable after the publish → first-push → amend →
/// force-push → amend → force-push arrangement, captured once per test
/// binary via [`SNAPSHOT`] and shared read-only across all `#[rstest]`
/// cases.
struct Snapshot {
    /// All `Kind::GitPatch` events on the GRASP after both force pushes.
    /// Expected count: 9 (2 original + 1 first push + 3 revision #1 +
    /// 3 revision #2).
    all_patch_events: Vec<Event>,

    /// Revision-root patch from the **first** force push — the patch for
    /// `commits[0]` (t3.md).  Carries `["t", "root"]`,
    /// `["t", "root-revision"]`, and an
    /// `["e", <original_root_id>, _, "reply"]` tag.
    revision_root: Event,

    /// Second patch in the **first** force-push revision — covers
    /// `commits[1]` (t4.md), authored by the maintainer.  Disambiguated
    /// from the original contributor patch for the same commit OID by
    /// author pubkey.
    revision_patch_2: Event,

    /// Tip patch in the **first** force-push revision — covers the
    /// first-amend commit OID.
    revision_tip: Event,

    /// Revision-root patch from the **second** force push.  Identified
    /// as the `Kind::GitPatch` event carrying `["t", "root-revision"]`
    /// (or alias `"revision-root"`) with the *latest* `created_at`.
    revision_2_root: Event,

    /// Tip patch in the **second** force-push revision — covers the
    /// second-amend commit OID.  Unique among all patch events on the
    /// GRASP because the OID is new.
    revision_2_tip: Event,

    /// Event ID of the original series root patch published by
    /// [`Harness::publish_patch_series`] (the patch carrying the
    /// `branch-name` tag).  Both revision roots' `["e", _, _, "reply"]`
    /// tags should equal this — never the first revision root's id.
    original_root_patch_id: EventId,

    /// Count of `KIND_PULL_REQUEST` events on the GRASP.  Must be 0 — force
    /// pushing on top of a patch-kind root must not produce a PR.
    pr_count: usize,

    /// Count of `KIND_PULL_REQUEST_UPDATE` events on the GRASP.  Must be 0 —
    /// a PR-update event is only valid when the root proposal is PR-kind.
    pr_update_count: usize,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary via
/// [`SNAPSHOT`] and hand each case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("git_push_pr::patch_update_force fixture: capture_snapshot failed"),
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
            display_name: Some("patch-update-force maintainer".into()),
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
    // letter.  `--force-patch` is set inside the helper, decoupling the test
    // from the default-kind heuristic in `ngit send`.
    let series = harness
        .publish_patch_series(&published, PublishPatchSeriesOpts::default())
        .await?;

    // --- 4. Identify the original series root --------------------------------
    //
    // The root is the first patch in the series — the only one whose
    // `branch-name` tag is set (added by `generate_patch_event` when
    // `events.is_empty()` in the per-commit loop).  Its event ID becomes
    // the `in_reply_to` anchor for the force-push revision.
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
    let shorthand = &original_root_patch.id.to_hex()[..8];
    let remote_branch = format!("pr/{}({})", series.branch_name, shorthand);

    // --- 5. Maintainer clones ------------------------------------------------
    //
    // `AsMaintainer` reuses `published.maintainer_nsec` — the maintainer has
    // push permission on any proposal branch (push.rs:481-484).
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

    // --- 7. First push (fast-forward) ----------------------------------------
    //
    // `nostr_push` ticks one unix second before pushing.  Adds one
    // `Kind::GitPatch` event covering the new commit.  Total patch events: 3.
    maintainer_clone
        .nostr_push(["origin", &remote_branch])
        .await
        .context("nostr_push of maintainer follow-up (first push) failed")?;

    // --- 8. Amend the tip commit ---------------------------------------------
    //
    // Write new content and amend so that the commit OID changes.  The amend
    // is local-only — no push — so it creates the non-fast-forward divergence
    // that requires `-f` on the next push.
    std::fs::write(
        maintainer_clone.dir().join("maintainer-update.md"),
        "maintainer follow-up (amended)\n",
    )
    .context("failed to write amended maintainer-update.md")?;
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
                "amended follow-up on patch series",
                "--no-gpg-sign",
            ],
            "git commit --amend",
        )
        .await?;

    let amended_tip_oid = maintainer_clone
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after amend")?;

    // --- 9. Force push (revision #1) -----------------------------------------
    //
    // `nostr_push` ticks another unix second before pushing (mandatory per
    // the test-harness timing rule).  The `-f` flag causes git to prefix the
    // refspec with `+`, which `push.rs:485` detects as a force push.  The
    // remote helper resolves all commits ahead of `main` (3 commits: the two
    // original contributor commits plus the amended maintainer commit) and
    // calls `generate_cover_letter_and_patch_events` with
    // `root_proposal = Some(original_root_patch)`, producing 3 new
    // `Kind::GitPatch` events as a revision.  Total patch events on GRASP: 6.
    maintainer_clone
        .nostr_push(["-f", "origin", &remote_branch])
        .await
        .context("nostr_push -f (force push after amend) failed")?;

    // --- 10. Amend the tip commit again --------------------------------------
    //
    // Second amend — produces another non-fast-forward divergence requiring
    // a second force push.  The new commit OID is distinct from the first
    // amend OID, so the resulting revision tip patch is identifiable by its
    // `commit` tag alone.
    std::fs::write(
        maintainer_clone.dir().join("maintainer-update.md"),
        "maintainer follow-up (amended twice)\n",
    )
    .context("failed to write twice-amended maintainer-update.md")?;
    maintainer_clone
        .git_ok(
            ["add", "maintainer-update.md"],
            "git add maintainer-update.md (second amend step)",
        )
        .await?;
    maintainer_clone
        .git_ok(
            [
                "commit",
                "--amend",
                "-m",
                "twice-amended follow-up on patch series",
                "--no-gpg-sign",
            ],
            "git commit --amend (second)",
        )
        .await?;

    let amended_tip_2_oid = maintainer_clone
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after second amend")?;
    if amended_tip_2_oid == amended_tip_oid {
        return Err(anyhow!(
            "second amend produced the same commit OID as the first amend \
             ({amended_tip_2_oid}); the second force push will be a no-op and \
             the test will not exercise the intended path",
        ));
    }

    // --- 11. Force push (revision #2) ----------------------------------------
    //
    // Same code path as step 9, but now there is *already* a revision on the
    // relay.  The question this step answers: does the new revision root
    // reply to the *original* root patch, or to the first revision root?
    //
    // Prediction (from reading lib/utils.rs:208 + push.rs:472-502): the
    // original root.  `get_all_proposals` filters out revision roots, so
    // `find_proposal_and_patches_by_branch_name` can only return the
    // original.  Asserted empirically by
    // `revision_2_root_replies_to_original_root` below.
    //
    // Adds 3 more `Kind::GitPatch` events.  Total on GRASP: 9.
    maintainer_clone
        .nostr_push(["-f", "origin", &remote_branch])
        .await
        .context("nostr_push -f (second force push after second amend) failed")?;

    // --- 12. Query GRASP state -----------------------------------------------
    let all_patch_events = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitPatch))
        .await?;

    let pr_count = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST))
        .await?
        .len();

    let pr_update_count = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST_UPDATE))
        .await?
        .len();

    // --- 13. Identify the two revision roots ---------------------------------
    //
    // Both force pushes produce a revision root carrying
    // `t=root-revision` (or alias `revision-root`).  Disambiguate by
    // `created_at`: earlier = revision #1, later = revision #2.  The
    // mandatory one-second tick in `nostr_push` guarantees strictly
    // ordered timestamps.
    let mut revision_roots: Vec<Event> = all_patch_events
        .iter()
        .filter(|e| {
            tag_values_multiple(e, "t")
                .iter()
                .any(|v| v == "root-revision" || v == "revision-root")
        })
        .cloned()
        .collect();
    revision_roots.sort_by_key(|e| e.created_at);
    if revision_roots.len() != 2 {
        return Err(anyhow!(
            "expected exactly 2 Kind::GitPatch events on GRASP carrying \
             t={{root-revision,revision-root}} (one per force push); got {}. \
             all {} patch event ids: {:?}",
            revision_roots.len(),
            all_patch_events.len(),
            all_patch_events
                .iter()
                .map(|e| e.id.to_hex())
                .collect::<Vec<_>>(),
        ));
    }
    let revision_2_root = revision_roots.pop().expect("len == 2 checked above");
    let revision_root = revision_roots.pop().expect("len == 2 checked above");
    if revision_root.id == revision_2_root.id {
        return Err(anyhow!(
            "the two revision roots have identical event ids ({}); \
             the second force push appears to have re-published the first \
             revision root verbatim, defeating this test's purpose",
            revision_root.id.to_hex(),
        ));
    }

    // --- 14. Identify the revision-series mid + tip patches ------------------

    // (a) Intermediate patch in revision #1 — covers `commits[1]` (t4.md),
    //     authored by the maintainer.  The original series also has a patch
    //     for this commit OID (authored by the contributor), so disambiguate
    //     by author and (implicitly) by created_at via the revision #1
    //     window.  Here we just pick the earlier maintainer-authored patch
    //     for `commits[1]`; the second force push will have published a
    //     later one which we don't bind to a variable.
    let second_commit_oid = series
        .commits
        .get(1)
        .cloned()
        .context("series.commits has fewer than 2 entries; expected at least 2")?;
    let mut maintainer_patches_for_c2: Vec<Event> = all_patch_events
        .iter()
        .filter(|e| {
            e.pubkey == maintainer_pubkey
                && tag_value(e, "commit").as_deref() == Some(second_commit_oid.as_str())
        })
        .cloned()
        .collect();
    maintainer_patches_for_c2.sort_by_key(|e| e.created_at);
    let revision_patch_2 = maintainer_patches_for_c2.first().cloned().ok_or_else(|| {
        anyhow!(
            "no Kind::GitPatch event authored by maintainer ({}) with \
             commit tag = {second_commit_oid} found on GRASP after force push",
            maintainer_pubkey.to_hex(),
        )
    })?;

    // (b) Tip patch from revision #1 — covers the first-amend commit OID,
    //     which appears in exactly one patch event on the GRASP (the
    //     pre-amend OID was never re-published).
    let revision_tip = all_patch_events
        .iter()
        .find(|e| tag_value(e, "commit").as_deref() == Some(amended_tip_oid.as_str()))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no Kind::GitPatch event with commit tag = {amended_tip_oid} \
                 found on GRASP after force push",
            )
        })?;

    // (c) Tip patch from revision #2 — covers the second-amend commit OID,
    //     also unique among all events on the GRASP.
    let revision_2_tip = all_patch_events
        .iter()
        .find(|e| tag_value(e, "commit").as_deref() == Some(amended_tip_2_oid.as_str()))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no Kind::GitPatch event with commit tag = {amended_tip_2_oid} \
                 found on GRASP after second force push",
            )
        })?;

    Ok(Snapshot {
        all_patch_events,
        revision_root,
        revision_patch_2,
        revision_tip,
        revision_2_root,
        revision_2_tip,
        original_root_patch_id,
        pr_count,
        pr_update_count,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Exactly 9 `Kind::GitPatch` events on the GRASP: 2 from
/// `publish_patch_series`, 1 from the first (FF) push, 3 from the first
/// force-push revision, and 3 from the second force-push revision.
///
/// A count of 8 means one of the force pushes revised only the changed
/// commit rather than re-publishing the full series.  A count > 9 means
/// spurious duplicates.
#[rstest]
#[tokio::test]
async fn nine_patch_events_total(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.all_patch_events.len(),
        9,
        "expected exactly 9 Kind::GitPatch events on GRASP \
         (2 original + 1 first push + 3 revision #1 + 3 revision #2); \
         got {} (event ids: {:?})",
        s.all_patch_events.len(),
        s.all_patch_events
            .iter()
            .map(|e| e.id.to_hex())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Zero `KIND_PULL_REQUEST` events — force pushing on top of a patch-kind
/// root must not produce a PR.
#[rstest]
#[tokio::test]
async fn zero_pr_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 0,
        "expected zero KIND_PULL_REQUEST events on GRASP after force pushing on top \
         of a patch-kind proposal; got {}",
        s.pr_count,
    );
    Ok(())
}

/// Zero `KIND_PULL_REQUEST_UPDATE` events — a PR-update is only valid when
/// the root proposal is itself PR-kind.
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

/// Revision root carries `["t", "root"]` — it is a new proposal root from
/// the threading perspective, even though it also carries a back-reference
/// via `revision-root`.
#[rstest]
#[tokio::test]
async fn revision_root_has_t_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert!(
        tag_values_multiple(&s.revision_root, "t")
            .iter()
            .any(|v| v == "root"),
        "revision root should carry `t root`; t tags: {:?}",
        tag_values_multiple(&s.revision_root, "t"),
    );
    Ok(())
}

/// Revision root carries `["t", "root-revision"]` or the alias
/// `["t", "revision-root"]` — the marker downstream clients use to
/// distinguish revisions from first-time proposals.
#[rstest]
#[tokio::test]
async fn revision_root_has_t_revision_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let t_values = tag_values_multiple(&s.revision_root, "t");
    assert!(
        t_values
            .iter()
            .any(|v| v == "root-revision" || v == "revision-root"),
        "revision root should carry `t root-revision` (or alias `revision-root`); \
         t tags: {t_values:?}",
    );
    Ok(())
}

/// Revision root carries a 4-slot `["e", <original_root_patch_id>, _, "reply"]`
/// tag linking this revision back to the original proposal root.
#[rstest]
#[tokio::test]
async fn revision_root_replies_to_original_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let reply_e = s
        .revision_root
        .tags
        .iter()
        .find(|t| {
            let v = t.as_slice();
            v.first().map(String::as_str) == Some("e")
                && v.len() == 4
                && v.get(3).map(String::as_str) == Some("reply")
        })
        .ok_or_else(|| {
            anyhow!(
                "revision root missing 4-slot `e ... reply` tag; \
                 all tags: {:?}",
                s.revision_root.tags,
            )
        })?;
    assert_eq!(
        reply_e.as_slice().get(1).map(String::as_str),
        Some(s.original_root_patch_id.to_hex().as_str()),
        "revision root's `e reply` should point at the original series root; \
         got {:?}, want {:?}",
        reply_e.as_slice().get(1),
        s.original_root_patch_id.to_hex(),
    );
    Ok(())
}

/// Tip patch carries a 4-slot `["e", <revision_root_id>, _, "root"]` tag —
/// the thread root for the entire revision series is the revision root patch,
/// not the original proposal root.
#[rstest]
#[tokio::test]
async fn tip_patch_root_is_revision_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let root_e = s
        .revision_tip
        .tags
        .iter()
        .find(|t| {
            let v = t.as_slice();
            v.first().map(String::as_str) == Some("e")
                && v.len() == 4
                && v.get(3).map(String::as_str) == Some("root")
        })
        .ok_or_else(|| {
            anyhow!(
                "revision tip patch missing 4-slot `e ... root` tag; \
                 all tags: {:?}",
                s.revision_tip.tags,
            )
        })?;
    assert_eq!(
        root_e.as_slice().get(1).map(String::as_str),
        Some(s.revision_root.id.to_hex().as_str()),
        "revision tip patch's `e root` should point at the revision root; \
         got {:?}, want {:?}",
        root_e.as_slice().get(1),
        s.revision_root.id.to_hex(),
    );
    Ok(())
}

/// Tip patch carries a 4-slot `["e", <revision_patch_2_id>, _, "reply"]`
/// tag — the immediate predecessor in the revision series is the second
/// patch, not the revision root.
#[rstest]
#[tokio::test]
async fn tip_patch_replies_to_second_patch(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let reply_e = s
        .revision_tip
        .tags
        .iter()
        .find(|t| {
            let v = t.as_slice();
            v.first().map(String::as_str) == Some("e")
                && v.len() == 4
                && v.get(3).map(String::as_str) == Some("reply")
        })
        .ok_or_else(|| {
            anyhow!(
                "revision tip patch missing 4-slot `e ... reply` tag; \
                 all tags: {:?}",
                s.revision_tip.tags,
            )
        })?;
    assert_eq!(
        reply_e.as_slice().get(1).map(String::as_str),
        Some(s.revision_patch_2.id.to_hex().as_str()),
        "revision tip patch's `e reply` should point at the second patch in the \
         revision series (not the revision root); got {:?}, want {:?}",
        reply_e.as_slice().get(1),
        s.revision_patch_2.id.to_hex(),
    );
    Ok(())
}

/// The **second** revision root's 4-slot `["e", _, _, "reply"]` tag points
/// at the **original** root patch — *not* at the first revision root.
///
/// Why: `get_all_proposals` (lib/utils.rs:208) filters events for which
/// `event_is_revision_root` is true.  The map it builds — keyed by
/// proposal id — therefore never contains a revision root.  When the
/// second force push calls `find_proposal_and_patches_by_branch_name`
/// (push.rs:472) the only candidate that can match the branch is the
/// original proposal root, so that event becomes `effective_root` and
/// flows down to `generate_patch_event` as `root_proposal_id`
/// (git_events.rs:252), which writes its id into the new revision root's
/// `e reply` tag.
///
/// If this assertion ever fails with the tag pointing at
/// `s.revision_root.id`, it means the lookup or filter logic has changed
/// to surface revision roots as proposals — at which point this test, the
/// docstring at the top of the module, and the comment block in section
/// 11 (revision #2 force push) of `capture_snapshot` all need revisiting
/// together.
#[rstest]
#[tokio::test]
async fn revision_2_root_replies_to_original_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let reply_e = s
        .revision_2_root
        .tags
        .iter()
        .find(|t| {
            let v = t.as_slice();
            v.first().map(String::as_str) == Some("e")
                && v.len() == 4
                && v.get(3).map(String::as_str) == Some("reply")
        })
        .ok_or_else(|| {
            anyhow!(
                "second revision root missing 4-slot `e ... reply` tag; \
                 all tags: {:?}",
                s.revision_2_root.tags,
            )
        })?;
    let got = reply_e.as_slice().get(1).map(String::as_str);
    let want_original = s.original_root_patch_id.to_hex();
    let other_candidate = s.revision_root.id.to_hex();
    assert_eq!(
        got,
        Some(want_original.as_str()),
        "second revision root's `e reply` should point at the ORIGINAL root \
         patch ({want_original}), not the first revision root \
         ({other_candidate}); got {got:?}.\n\n\
         If got == first-revision-root id, the second force push is anchoring \
         on the latest revision instead of the original proposal — re-check \
         the filter in `get_all_proposals` (lib/utils.rs:208) and the lookup \
         in `find_proposal_and_patches_by_branch_name` (push.rs:472).",
    );
    Ok(())
}

/// The **second** revision tip's 4-slot `["e", _, _, "root"]` tag points
/// at the **second** revision root — i.e. each revision is its own
/// self-contained thread anchored on its own revision root, even though
/// every revision root's `e reply` resolves all the way back to the
/// original proposal.
#[rstest]
#[tokio::test]
async fn revision_2_tip_root_is_revision_2_root(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let root_e = s
        .revision_2_tip
        .tags
        .iter()
        .find(|t| {
            let v = t.as_slice();
            v.first().map(String::as_str) == Some("e")
                && v.len() == 4
                && v.get(3).map(String::as_str) == Some("root")
        })
        .ok_or_else(|| {
            anyhow!(
                "second revision tip patch missing 4-slot `e ... root` tag; \
                 all tags: {:?}",
                s.revision_2_tip.tags,
            )
        })?;
    assert_eq!(
        root_e.as_slice().get(1).map(String::as_str),
        Some(s.revision_2_root.id.to_hex().as_str()),
        "second revision tip patch's `e root` should point at the second \
         revision root ({}); got {:?}.  If it points at the first revision \
         root ({}) or the original root ({}), the per-revision thread \
         boundary has been broken.",
        s.revision_2_root.id.to_hex(),
        root_e.as_slice().get(1),
        s.revision_root.id.to_hex(),
        s.original_root_patch_id.to_hex(),
    );
    Ok(())
}
