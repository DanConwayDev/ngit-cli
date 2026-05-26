//! Coverage for `git push` on top of an existing **patch-kind** proposal.
//!
//! Scenario: a fresh contributor publishes a patch-series proposal via
//! `ngit send --force-patch` (no cover letter). Someone with push
//! permission — here the maintainer, for convenience: the contributor's
//! nsec is not exposed by [`Harness::publish_patch_series`] — clones,
//! checks out the proposal's advertised remote-tracking branch
//! (`pr/<branch>(<shorthand>)`), adds a commit, and runs plain `git
//! push`.  The remote helper must produce **another `Kind::GitPatch`
//! event**, never a `KIND_PULL_REQUEST` and never a
//! `KIND_PULL_REQUEST_UPDATE`.
//!
//! ## Why this is the load-bearing path
//!
//! `generate_patches_or_pr_event_or_pr_updates` in
//! `src/bin/git_remote_nostr/push.rs:651-658` decides between PR-kind
//! and patch-kind output:
//!
//! ```text
//! let use_pr = parent_is_pr
//!     || commits_too_big
//!     || has_submodules
//!     || (root_proposal.is_none() && repo_has_grasp_server);
//! ```
//!
//! With the existing patch series as `root_proposal`:
//! - `parent_is_pr = false` (the root is `Kind::GitPatch`, not
//!   `KIND_PULL_REQUEST`),
//! - `root_proposal.is_none()` is `false`,
//!
//! so `use_pr = false` and the push falls through to the patch-kind
//! emitter.  The actual code path taken for a fast-forward update is
//! `push.rs:557-580`'s per-commit `generate_patch_event` loop — the
//! `use_pr` check above is the upstream guard that keeps this test
//! honest if the loop ever gets refactored to share the PR/update
//! branch.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`) + one GRASP (`"repo"`).
//! 2. Maintainer publishes the repo via [`Harness::publish_repo`].
//! 3. A fresh contributor publishes a patch-series proposal via
//!    [`Harness::publish_patch_series`] with default opts (two commits, no
//!    cover letter) — this lands two `Kind::GitPatch` events on the GRASP.
//! 4. Maintainer clones the repo via [`Harness::clone_published_repo`] with
//!    [`CloneLogin::AsMaintainer`]. `list.rs:235-256` advertises the patch
//!    branch as `refs/heads/pr/<branch>(<shorthand>)` to anyone who is not the
//!    patch author, so the clone receives a matching
//!    `refs/remotes/origin/pr/<branch>(<shorthand>)`.
//! 5. Maintainer runs `git checkout pr/<branch>(<shorthand>)` — git
//!    auto-creates a tracking local branch.
//! 6. Maintainer commits one new file on that branch.
//! 7. Maintainer runs `git push origin pr/<branch>(<shorthand>)` (no `-f`, no
//!    `-u`). [`Repo::nostr_push`] ticks the harness clock forward one whole
//!    second beforehand to keep `created_at` values strictly ordered.
//!
//! ## Coverage (one `#[rstest]` per case)
//!
//! - `three_patch_events_total` — two original patches plus one push update
//!   equals exactly three `Kind::GitPatch` events on the GRASP.
//! - `zero_pr_events` — no `KIND_PULL_REQUEST` events were emitted.
//! - `zero_pr_update_events` — no `KIND_PULL_REQUEST_UPDATE` events were
//!   emitted.
//! - `update_patch_authored_by_maintainer` — the new patch event is signed by
//!   the maintainer (sanity check that the push went through as the cloned
//!   identity).
//! - `update_patch_commit_tag_is_new_tip` — the new patch's `commit` tag equals
//!   the maintainer's new commit OID.

use std::sync::Arc;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishPatchSeriesOpts,
    PublishRepoOpts, event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for this test repo — distinct from every other
/// `git_push_pr` scenario so the shared vanilla relay's REQ surface
/// stays uncontaminated across `cargo test --test git_push_pr`.
const IDENTIFIER: &str = "git-push-pr-patch-update";

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Everything observable after the publish-then-push arrangement, captured
/// once via [`SNAPSHOT`] and shared read-only across the `#[rstest]` cases.
struct Snapshot {
    /// All `Kind::GitPatch` events on the GRASP after the maintainer's
    /// push.  Must contain exactly three events (case
    /// `three_patch_events_total`): two from the initial
    /// `publish_patch_series` and one from the maintainer's push.
    patch_events: Vec<Event>,

    /// `KIND_PULL_REQUEST` count on the GRASP.  Must be 0 — pushing on
    /// top of a patch-kind root must not produce a PR (case
    /// `zero_pr_events`).
    pr_count: usize,

    /// `KIND_PULL_REQUEST_UPDATE` count on the GRASP.  Must be 0 — a
    /// PR-update event is only legal when the root is itself a PR (case
    /// `zero_pr_update_events`).
    pr_update_count: usize,

    /// Maintainer pubkey — the cloned identity that did the push.  The
    /// new patch event must be signed by them
    /// (`update_patch_authored_by_maintainer`).
    maintainer_pubkey: PublicKey,

    /// Original contributor pubkey — the one who ran
    /// `publish_patch_series`.  Used to identify the new patch event as
    /// "the one not authored by the contributor".
    contributor_pubkey: PublicKey,

    /// Commit OID of the new commit the maintainer added on top of the
    /// patch series before pushing.  The push-emitted patch's `commit`
    /// tag must equal this (`update_patch_commit_tag_is_new_tip`).
    update_commit_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("git_push_pr::patch_update fixture: capture_snapshot failed"),
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
            display_name: Some("patch-update maintainer".into()),
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
    // letter.  `--force-patch` is mandatory and load-bearing inside the
    // helper (decouples the test from the default-kind heuristic in
    // `src/bin/ngit/sub_commands/send.rs`).
    let series = harness
        .publish_patch_series(&published, PublishPatchSeriesOpts::default())
        .await?;
    let contributor_pubkey = series.author_pubkey;

    // The series has no cover letter, so the "root" patch event for
    // ref-name purposes is the patch carrying the `branch-name` tag.  Its
    // event id supplies the 8-char shorthand that `list.rs:235` builds
    // `pr/<branch>(<shorthand>)` from.
    let root_patch = series
        .patch_events
        .iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(series.branch_name.as_str()))
        .with_context(|| {
            format!(
                "no root patch event (with `branch-name` = {:?}) found among \
                 series.patch_events — has the harness changed how it tags the root?",
                series.branch_name,
            )
        })?;
    let shorthand = &root_patch.id.to_hex()[..8];
    let remote_branch = format!("pr/{}({})", series.branch_name, shorthand);

    // --- 4. Maintainer clones (a brand-new working tree) ---------------------
    //
    // `AsMaintainer` reuses `published.maintainer_nsec` rather than
    // minting a fresh account, so this clone is authorised to push on
    // any proposal branch (the permission check in
    // `push.rs:481-484` accepts repo maintainers in addition to the
    // proposal author).
    let maintainer_clone = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    // Sanity-check that the patch branch is actually advertised to the
    // maintainer — if `list.rs` ever stops advertising patches as
    // `pr/<branch>(<shorthand>)`, the `git checkout` below will fail
    // with a confusing "pathspec did not match" error.  Catch it here
    // with a clearer message instead.
    let snap = maintainer_clone
        .snapshot()
        .context("snapshotting maintainer clone before checkout")?;
    let remote_ref = format!("refs/remotes/origin/{remote_branch}");
    snap.refs.get(&remote_ref).with_context(|| {
        format!(
            "{remote_ref} missing from maintainer clone after `git clone` — \
             list.rs no longer advertises patch series as pr/<branch>(<shorthand>) \
             for non-author viewers?  Available refs: {:?}",
            snap.refs.keys().collect::<Vec<_>>(),
        )
    })?;

    // --- 5. Maintainer checks out the proposal branch ------------------------
    //
    // Plain `git checkout pr/feature-1(<short>)` triggers git's DWIM
    // behaviour: with no matching local branch and exactly one matching
    // remote-tracking ref, it creates a local branch of the same name
    // with the remote-tracking ref as upstream.
    maintainer_clone
        .git_ok(
            ["checkout", &remote_branch],
            &format!("git checkout {remote_branch}"),
        )
        .await?;

    // --- 6. Maintainer commits one new file on top of the series tip ---------
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

    let update_commit_oid = maintainer_clone
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after maintainer-update.md commit")?;

    // --- 7. Maintainer pushes via the nostr:// remote ------------------------
    //
    // `nostr_push` ticks the clock so the new patch event lands in a
    // strictly later `created_at` second than the series' own events.
    // Mandatory per the test-harness "Timing rule" (see
    // `docs/architecture/test-harness.md`).
    maintainer_clone
        .nostr_push(["origin", &remote_branch])
        .await
        .context("nostr_push of maintainer follow-up commit failed")?;

    // --- 8. Capture GRASP state after the push -------------------------------
    let patch_events = harness
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

    Ok(Snapshot {
        patch_events,
        pr_count,
        pr_update_count,
        maintainer_pubkey,
        contributor_pubkey,
        update_commit_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Case 1: exactly three `Kind::GitPatch` events on the GRASP — two from
/// the original `publish_patch_series` (t3.md and t4.md) and one from the
/// maintainer's push (maintainer-update.md).
///
/// A count of 2 means the push produced no patch event at all (the
/// fast-forward path in `push.rs:557-580` short-circuited).  A count of 4
/// or more means the push emitted spurious extras — typically the
/// cover-letter-plus-patches shape from
/// `generate_cover_letter_and_patch_events` mis-firing on a single
/// follow-up commit.
#[rstest]
#[tokio::test]
async fn three_patch_events_total(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.patch_events.len(),
        3,
        "expected exactly 3 Kind::GitPatch events on GRASP (2 from publish_patch_series + \
         1 from the maintainer push); got {} (events: {:?})",
        s.patch_events.len(),
        s.patch_events
            .iter()
            .map(|e| (e.id.to_hex(), e.pubkey.to_hex()))
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Case 2: zero `KIND_PULL_REQUEST` events on the GRASP.
///
/// Pushing on top of a patch-kind root must keep the proposal patch-kind.
/// A non-zero count here means `use_pr` in `push.rs:655` evaluated to
/// `true` when it should not have — either `parent_is_pr` mis-detected
/// the kind of the existing root, or the
/// `(root_proposal.is_none() && repo_has_grasp_server)` term fired
/// despite `root_proposal` being `Some`.
#[rstest]
#[tokio::test]
async fn zero_pr_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 0,
        "expected zero KIND_PULL_REQUEST events on GRASP after pushing on top of a \
         patch-kind proposal; got {} — did the push convert the patch series into a PR?",
        s.pr_count,
    );
    Ok(())
}

/// Case 3: zero `KIND_PULL_REQUEST_UPDATE` events on the GRASP.
///
/// `KIND_PULL_REQUEST_UPDATE` (kind 1619) is only emitted when the root
/// proposal is itself a `KIND_PULL_REQUEST` — see `push.rs:651` and the
/// PR-update construction in `git_events.rs` `pr_update_specific_tags`.
/// A non-zero count here means a PR-update was authored against a
/// patch-kind root, which is a protocol error.
#[rstest]
#[tokio::test]
async fn zero_pr_update_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events on GRASP; got {} — was a kind-1619 \
         event authored against a patch-kind root?",
        s.pr_update_count,
    );
    Ok(())
}

/// Case 4: exactly one `Kind::GitPatch` event on the GRASP is authored
/// by the maintainer; the rest are authored by the original contributor.
///
/// Sanity check that the maintainer's push was signed by the cloned
/// identity (not, say, by the contributor's key surviving from an
/// earlier login).  Also rules out the regression where the push silently
/// no-ops and the test still passes "exactly 3 patch events" because the
/// contributor's three remained.
#[rstest]
#[tokio::test]
async fn update_patch_authored_by_maintainer(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let by_maintainer: Vec<&Event> = s
        .patch_events
        .iter()
        .filter(|e| e.pubkey == s.maintainer_pubkey)
        .collect();
    let by_contributor: Vec<&Event> = s
        .patch_events
        .iter()
        .filter(|e| e.pubkey == s.contributor_pubkey)
        .collect();
    assert_eq!(
        by_maintainer.len(),
        1,
        "expected exactly 1 Kind::GitPatch event authored by the maintainer; got {} \
         (maintainer={}, contributor={})",
        by_maintainer.len(),
        s.maintainer_pubkey.to_hex(),
        s.contributor_pubkey.to_hex(),
    );
    assert_eq!(
        by_contributor.len(),
        2,
        "expected exactly 2 Kind::GitPatch events authored by the original contributor; got {}",
        by_contributor.len(),
    );
    Ok(())
}

/// Case 5: the maintainer-authored patch event's `commit` tag equals the
/// maintainer's new commit OID.
///
/// The `commit` tag is what `get_commit_id_from_patch` reads to identify
/// the patch's commit in subsequent fetch / checkout flows.  A wrong
/// value silently makes the new commit unreachable through the patch
/// thread even though the event itself is published.
#[rstest]
#[tokio::test]
async fn update_patch_commit_tag_is_new_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let by_maintainer = s
        .patch_events
        .iter()
        .find(|e| e.pubkey == s.maintainer_pubkey)
        .context("no maintainer-authored Kind::GitPatch event found on GRASP")?;
    assert_eq!(
        tag_value(by_maintainer, "commit").as_deref(),
        Some(s.update_commit_oid.as_str()),
        "maintainer-authored patch's `commit` tag should equal the new commit OID; \
         got {:?}, want {:?}",
        tag_value(by_maintainer, "commit"),
        s.update_commit_oid,
    );
    Ok(())
}
