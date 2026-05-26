//! End-to-end coverage of a force-push `git push -f origin pr/feature` after
//! a rebase onto a newer `main` (push.rs:480-507 — the force-push path inside
//! `process_proposal_refspecs`).
//!
//! ## What this test covers
//!
//! This is the force-push / rebase sibling of `ff_update.rs`.  Steps 1–6
//! mirror `ff_update.rs` exactly (original PR via `-u` push); steps 7–10 are
//! the rebase-specific divergence.
//!
//! The key invariant under test (assertion 3) is the inverse of the 840c581
//! fix in `ff_update.rs`:
//!
//! > After a rebase the `merge-base` tag on the PR update event must equal
//! > the **new** fork point (the `origin/main` tip at rebase time), **not**
//! > the original fork point preserved by the 840c581 path.
//!
//! In the force-push path `get_main_or_master_branch()` resolves to
//! `origin/main` (updated by the contributor's `git fetch origin`), so
//! `ahead` is computed relative to the new main tip.  The merge-base must
//! therefore be `get_commit_parent(first_commit)` — the parent of the first
//! rebased PR commit — which equals the new main tip (the rebase target).
//! Using the old PR event's `merge-base` tag (path taken by the 840c581 fix)
//! would give the pre-rebase fork point, which is stale after a rebase.
//!
//! ## Arrangement
//!
//! Steps 1–6 mirror `ff_update.rs` exactly; steps 7–10 are the
//! force-push-after-rebase divergence.
//!
//! 1. Harness: one vanilla relay (`"default"`) + one GRASP server (`"repo"`).
//! 2. Maintainer publishes the repo via [`Harness::publish_repo`].
//! 3. Fresh contributor clones and logs in.
//! 4. Contributor checks out `pr/feature` and makes two commits (`t1.md`,
//!    `t2.md`).  The parent of `t1.md`'s commit is `published.initial_oid` —
//!    that is `original_fork_point_oid`.
//! 5. **Maintainer first advance of `main`** — creates a gap so the original
//!    PR's `merge-base` is `initial_oid` while the then-current `main` tip is
//!    newer (same rationale as `ff_update.rs` step 5).
//! 6. Contributor runs `git push -u origin pr/feature` (first push).  This
//!    creates the **original KIND_PULL_REQUEST**; its `merge-base` tag equals
//!    `original_fork_point_oid` = `published.initial_oid`.
//! 7. **Maintainer second advance of `main`** — this is the commit the
//!    contributor will rebase onto.
//! 8. **Contributor fetches `origin` and rebases `pr/feature` onto
//!    `origin/main`.**  After the rebase: `HEAD~2` = new main tip =
//!    `rebased_merge_base_oid`.
//! 9. Contributor commits `t3.md` (the update commit, on top of the rebased
//!    branch tip).
//! 10. Contributor runs `git push -f origin pr/feature` (force push).  This is
//!     the **PR Update** — the act under test.
//! 11. [`capture_snapshot`] reads all events and git refs; harness drops. Each
//!     `#[rstest]` case asserts on one slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. **one_pr_one_update** — the force push produced exactly one
//!    KIND_PULL_REQUEST_UPDATE and did not emit a new KIND_PULL_REQUEST.
//! 2. **update_e_tag_points_at_original_pr** — the update event's uppercase `E`
//!    tag equals the original PR event ID.
//! 3. **update_merge_base_is_new_fork_point** — the update event's `merge-base`
//!    tag equals `rebased_merge_base_oid` (the new main tip) and **not**
//!    `original_fork_point_oid`.  This is the direct counterpart of
//!    `ff_update.rs` assertion 3: the 840c581 "preserve original merge-base"
//!    logic must **not** apply after a force-push rebase.
//! 4. **update_c_tag_is_new_tip** — the update event's `c` tag equals the tip
//!    OID after committing `t3.md` on the rebased branch.
//! 5. **contributor_remote_tracking_advanced_to_new_tip** — the contributor's
//!    `refs/remotes/origin/pr/feature` resolves to the new tip OID.
//! 6. **grasp_has_refs_nostr_for_update** — the GRASP bare repo carries
//!    `refs/nostr/<update_event_id>` resolving to the new tip OID.
//! 7. **non_force_push_rejected** — a plain `git push origin pr/feature`
//!    (without `-f`) after the rebase must be rejected by the remote; the force
//!    flag is required to update a branch whose history has been rewritten.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for the test repo — distinct from the other git_push_pr
/// scenarios to avoid cross-test relay pollution on the shared vanilla relay.
const IDENTIFIER: &str = "git-push-pr-rebase-update";

/// Feature branch name; pushed as `pr/feature`.
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot — all observable side-effects captured once and shared
// ---------------------------------------------------------------------------

/// Everything observable after the two-push arrangement (original PR via `-u`,
/// then force-push update after a rebase), captured during
/// [`capture_snapshot`] and shared read-only across the six `#[rstest]`
/// cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST_UPDATE event produced by the force push.
    /// Assertions 2–4 read tag values from here.
    pr_update_event: Event,

    /// Total KIND_PULL_REQUEST events authored by the contributor on the GRASP
    /// after both pushes.  Must equal 1 (assertion 1 — the force push must
    /// not have emitted a new PR).
    pr_count: usize,

    /// Total KIND_PULL_REQUEST_UPDATE events authored by the contributor on
    /// the GRASP after both pushes.  Must equal 1 (assertion 1).
    pr_update_count: usize,

    /// Hex-encoded event ID of the original KIND_PULL_REQUEST event.  The
    /// update's uppercase `E` tag must equal this (assertion 2).
    original_pr_event_id: String,

    /// OID of the original fork point before the rebase
    /// (`published.initial_oid`). Kept so that assertion 3 can verify
    /// `rebased_merge_base_oid != original_fork_point_oid`, making the
    /// merge-base assertion non-trivial.
    original_fork_point_oid: String,

    /// The new fork point after the rebase — the `origin/main` tip at rebase
    /// time (the second maintainer advance).  The update event's `merge-base`
    /// tag must equal this (assertion 3).
    rebased_merge_base_oid: String,

    /// Contributor's `pr/feature` tip OID after committing `t3.md` on the
    /// rebased branch.  The update's `c` tag and the remote-tracking ref must
    /// both equal this (assertions 4 & 5).
    update_tip_oid: String,

    /// `refs/remotes/origin/pr/feature` from the contributor's repo snapshot
    /// taken after the force push.  Must equal `update_tip_oid` (assertion 5).
    contributor_remote_tracking_oid: String,

    /// OID that `refs/nostr/<update_event_id>` resolves to in the GRASP bare
    /// repo.  Must equal `update_tip_oid` (assertion 6).
    grasp_update_ref_oid: String,

    /// `true` when the plain `git push origin pr/feature` (without `-f`)
    /// attempt made after the rebase was rejected by the remote.
    /// Expected to always be `true`; stored so that assertion 7 can fail the
    /// test if the push unexpectedly succeeds.
    non_force_push_was_rejected: bool,
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
                    .expect("git_push_pr::rebase_update fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- 1. Harness: one default relay + one GRASP server --------------------
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    // --- 2. Maintainer publishes the repo ------------------------------------
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("rebase-update maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    // --- 3. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "rebase-update contributor".into(),
            },
        )
        .await?;

    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after AsContributor login")?;
    let contributor_keys =
        Keys::parse(&contributor_nsec).context("contributor nostr.nsec is not a valid key")?;
    let contributor_pubkey = contributor_keys.public_key();

    // --- 4. Contributor: checkout pr/feature + two commits -------------------
    //
    // The parent of t1.md's commit is `published.initial_oid` — that is the
    // original fork point that the `merge-base` tag must NOT equal after the
    // rebase.
    contributor
        .git_ok(
            ["checkout", "-b", &format!("pr/{BRANCH}")],
            &format!("git checkout -b pr/{BRANCH}"),
        )
        .await?;

    std::fs::write(contributor.dir().join("t1.md"), "some content\n")
        .context("failed to write t1.md")?;
    contributor
        .git_ok(["add", "t1.md"], "git add t1.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t1.md", "--no-gpg-sign"],
            "git commit t1.md",
        )
        .await?;

    std::fs::write(contributor.dir().join("t2.md"), "some content\n")
        .context("failed to write t2.md")?;
    contributor
        .git_ok(["add", "t2.md"], "git add t2.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t2.md", "--no-gpg-sign"],
            "git commit t2.md",
        )
        .await?;

    let original_fork_point_oid = published.initial_oid.clone();

    // --- 5. Maintainer: first advance of main --------------------------------
    //
    // Creates a gap between `original_fork_point_oid` (= initial_oid, the
    // fork point) and the current `main` tip.  Without this step the
    // merge-base assertion would be trivially satisfied even by a broken
    // implementation that substitutes `main` tip for the fork point.
    std::fs::write(publisher.dir().join("main-v1.md"), "content\n")
        .context("failed to write main-v1.md on publisher side")?;
    publisher
        .git_ok(["add", "main-v1.md"], "git add main-v1.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main (v1)", "--no-gpg-sign"],
            "git commit advance main (v1)",
        )
        .await?;
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("maintainer nostr_push to advance main (v1) failed")?;

    // --- 6. Contributor: first push — creates original KIND_PULL_REQUEST ----
    //
    // `nostr_push` ticks one whole unix second before pushing to avoid
    // created_at collisions.  `-u` sets upstream tracking so the second push
    // (without `-u`) can later advance the remote-tracking ref.
    contributor
        .nostr_push(["-u", "origin", &format!("pr/{BRANCH}")])
        .await
        .context("first nostr_push -u origin pr/feature failed")?;

    // Capture the original PR event so we have its ID and can verify that
    // reassignment below is correct.
    let original_pr_event = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on GRASP after first `git push -u origin pr/feature`",
        )?;
    let original_pr_event_id = original_pr_event.id.to_hex();

    // --- 7. Maintainer: second advance of main (the rebase target) ----------
    //
    // This is the commit the contributor will rebase onto.  After the rebase
    // its OID becomes the new fork point that must appear as `merge-base` in
    // the PR update event.
    std::fs::write(publisher.dir().join("main-v2.md"), "content\n")
        .context("failed to write main-v2.md on publisher side")?;
    publisher
        .git_ok(["add", "main-v2.md"], "git add main-v2.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main (v2)", "--no-gpg-sign"],
            "git commit advance main (v2)",
        )
        .await?;
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("maintainer nostr_push to advance main (v2) failed")?;

    // --- 8. Contributor: fetch + rebase onto origin/main --------------------
    //
    // `git fetch origin` pulls both new main commits via the nostr remote
    // helper. `git rebase origin/main` replays t1.md and t2.md on top of the
    // new main tip (main-v2 commit). No conflicts: the maintainer touched
    // main-v1.md / main-v2.md while the contributor touched t1.md / t2.md.
    //
    // After the rebase the topology is:
    //   [initial] → [main-v1] → [main-v2] → [rebased-t1] → [rebased-t2]
    //                                 ↑
    //                           HEAD~2 = new fork point
    contributor
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;
    contributor
        .git_ok(["rebase", "origin/main"], "git rebase origin/main")
        .await?;

    // Capture the new fork point immediately after the rebase (before adding
    // t3.md). With two rebased commits (rebased-t1, rebased-t2), HEAD~2 is
    // main-v2 — the new fork point.
    let rebased_merge_base_oid = contributor.rev_parse("HEAD~2").await?;

    // Sanity check: the rebase must have moved the fork point.
    if rebased_merge_base_oid == original_fork_point_oid {
        bail!(
            "setup invariant violated: rebased_merge_base_oid equals original_fork_point_oid \
             ({rebased_merge_base_oid}) — rebase did not change the fork point as expected"
        );
    }

    // --- 9. Contributor: third commit (t3.md) — the update commit -----------
    //
    // Added on top of the rebased branch. HEAD is now rebased-t2, so after
    // this commit:
    //   HEAD = t3, HEAD~1 = rebased-t2, HEAD~2 = rebased-t1, HEAD~3 = main-v2
    std::fs::write(contributor.dir().join("t3.md"), "more content\n")
        .context("failed to write t3.md")?;
    contributor
        .git_ok(["add", "t3.md"], "git add t3.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t3.md", "--no-gpg-sign"],
            "git commit t3.md",
        )
        .await?;

    let update_tip_oid = contributor
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after t3.md commit")?;

    // --- 10. Contributor: attempt plain push (no -f) — must be rejected ------
    //
    // After a rebase the local branch history has diverged from the remote
    // tracking ref (the original PR tip).  The remote must reject a non-force
    // push because it is not a fast-forward.  We capture the outcome so that
    // assertion 7 can verify the rejection, and we skip the force push below
    // when this attempt unexpectedly succeeds (so the remaining assertions can
    // still observe the event data from the successful push).
    let plain_push_out = contributor
        .git(["push", "origin", &format!("pr/{BRANCH}")])
        .output()
        .await
        .context("failed to spawn git push origin pr/feature (no -f)")?;
    let non_force_push_was_rejected = !plain_push_out.status.success();

    // --- 11. Contributor: force push — PR update after rebase ---------------
    //
    // `git push -f origin pr/feature` (via nostr_push ["-f", ...]). The
    // remote helper detects the refspec starts with '+' (force flag), looks
    // up the existing KIND_PULL_REQUEST for pr/feature, computes `ahead`
    // relative to `origin/main` (= main-v2, updated by the fetch), and
    // routes through `generate_patches_or_pr_event_or_pr_updates` with
    // `root_proposal = Some(original_pr_event)` and `is_force_push = true`.
    // The merge-base must be computed as `get_commit_parent(first_commit)` =
    // main-v2, NOT as the original PR's stale `merge-base` tag (initial_oid).
    //
    // Skipped when the plain push above unexpectedly succeeded — the data is
    // already on the remote so the subsequent snapshot reads still work.
    if non_force_push_was_rejected {
        contributor
            .nostr_push(["-f", "origin", &format!("pr/{BRANCH}")])
            .await
            .context("nostr_push -f origin pr/feature (force push after rebase) failed")?;
    }

    // --- 12. Capture contributor local state after the force push -----------
    let contributor_snap = contributor
        .snapshot()
        .context("capturing contributor snapshot after force push")?;
    let remote_tracking_ref = format!("refs/remotes/origin/pr/{BRANCH}");
    let contributor_remote_tracking_oid = contributor_snap
        .refs
        .get(&remote_tracking_ref)
        .with_context(|| {
            format!(
                "{remote_tracking_ref} missing from contributor refs after force push — \
                 update_remote_refs_pushed (push.rs:165-170) did not run"
            )
        })?
        .clone();

    // --- 13. Capture events from the GRASP after the force push -------------
    let pr_events_final = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count = pr_events_final.len();

    let pr_update_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_update_count = pr_update_events.len();
    let pr_update_event = pr_update_events.into_iter().next().context(
        "no KIND_PULL_REQUEST_UPDATE authored by contributor found on GRASP \
         after `git push -f origin pr/feature` (force push after rebase)",
    )?;

    // --- 14. Read the GRASP bare-repo ref before the harness drops ----------
    let update_event_id_hex = pr_update_event.id.to_hex();
    let grasp_update_ref_oid = harness
        .grasp("repo")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &update_event_id_hex)
        .await?;

    Ok(Snapshot {
        pr_update_event,
        pr_count,
        pr_update_count,
        original_pr_event_id,
        original_fork_point_oid,
        rebased_merge_base_oid,
        update_tip_oid,
        contributor_remote_tracking_oid,
        grasp_update_ref_oid,
        non_force_push_was_rejected,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST and one KIND_PULL_REQUEST_UPDATE
/// exist on the GRASP after both pushes.
///
/// A PR count > 1 means the force push published a new PR instead of an
/// update.  An update count > 1 indicates a duplicate-publish bug.
#[rstest]
#[tokio::test]
async fn one_pr_one_update(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 1,
        "expected exactly one KIND_PULL_REQUEST on GRASP after both pushes; \
         got {} — did the force push produce a new PR instead of an update?",
        s.pr_count,
    );
    assert_eq!(
        s.pr_update_count, 1,
        "expected exactly one KIND_PULL_REQUEST_UPDATE on GRASP after both pushes; \
         got {}",
        s.pr_update_count,
    );
    Ok(())
}

/// Assertion 2: the update event's uppercase `E` tag equals the original PR
/// event ID.
///
/// The force push after a rebase rewrites the commit OIDs but the original PR
/// event identity is unchanged — the uppercase `E` tag still links the update
/// back to the same PR thread.  A missing or wrong `E` tag would silently
/// orphan the update from the PR thread.
#[rstest]
#[tokio::test]
async fn update_e_tag_points_at_original_pr(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "E").as_deref(),
        Some(s.original_pr_event_id.as_str()),
        "update event uppercase `E` tag should equal original PR event ID; \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "E"),
        s.original_pr_event_id,
    );
    Ok(())
}

/// Assertion 3: after a force-push rebase the `merge-base` tag on the update
/// event equals the **new** fork point (`rebased_merge_base_oid`), not the
/// original one (`original_fork_point_oid`).
///
/// This is the direct counterpart of `ff_update.rs` assertion 3 and the
/// inverse of the 840c581 fix.  The 840c581 fix correctly preserves the
/// original merge-base for **fast-forward** pushes where the first new commit
/// sits on top of the existing PR tip.  But for a **force push after rebase**
/// the first rebased commit's parent IS the new fork point — using the
/// original PR's stale `merge-base` tag would give an incorrect, pre-rebase
/// value.
///
/// The precondition assertion (`rebased != original`) makes the test
/// non-trivial: if the rebase did nothing the two OIDs would be equal and the
/// test would only be checking a trivially-correct value.
#[rstest]
#[tokio::test]
async fn update_merge_base_is_new_fork_point(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    // Precondition: the rebase must have moved the fork point.
    assert_ne!(
        s.rebased_merge_base_oid, s.original_fork_point_oid,
        "setup invariant: rebased_merge_base_oid should differ from original_fork_point_oid \
         (both are {:?}) — the rebase did not change the fork point",
        s.rebased_merge_base_oid,
    );
    assert_eq!(
        tag_value(&s.pr_update_event, "merge-base").as_deref(),
        Some(s.rebased_merge_base_oid.as_str()),
        "update event `merge-base` tag should equal the new fork-point OID after rebase \
         (840c581 preserve-merge-base logic must not apply to force-push updates); \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "merge-base"),
        s.rebased_merge_base_oid,
    );
    Ok(())
}

/// Assertion 4: the update event's `c` tag equals the new tip OID (the
/// `t3.md` commit on the rebased branch).
///
/// The rebased commits have fresh OIDs; `t3.md` is committed on top of those.
/// The `c` tag must equal the new `HEAD` OID so that `get_commit_id_from_patch`
/// can locate the tip commit for checkout / apply operations.
#[rstest]
#[tokio::test]
async fn update_c_tag_is_new_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "c").as_deref(),
        Some(s.update_tip_oid.as_str()),
        "update event `c` tag should equal the new tip OID (t3.md on rebased branch); \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "c"),
        s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 5: the contributor's `refs/remotes/origin/pr/feature` points at
/// the new tip OID after the force push.
///
/// `update_remote_refs_pushed` (push.rs:165-170) must advance the
/// remote-tracking ref even for a force push.  The rebased commits are new
/// objects so this verifies the ref was updated to reflect the new tip, not
/// left at the original PR's tip.
#[rstest]
#[tokio::test]
async fn contributor_remote_tracking_advanced_to_new_tip(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.contributor_remote_tracking_oid, s.update_tip_oid,
        "contributor refs/remotes/origin/pr/{BRANCH} ({}) should point at new tip ({}); \
         was the remote-tracking ref advanced by the force push?",
        s.contributor_remote_tracking_oid, s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 6: the GRASP bare repo has `refs/nostr/<update_event_id>`
/// resolving to the new tip OID.
///
/// Proof that the git data for the force-push update (rebased commits +
/// t3.md) actually landed on the GRASP server, not just the kind-1619 nostr
/// event.  The rebased commits are new objects that were not present before
/// the force push.
#[rstest]
#[tokio::test]
async fn grasp_has_refs_nostr_for_update(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_update_ref_oid, s.update_tip_oid,
        "GRASP refs/nostr/<update_event_id> resolves to {} but expected update tip {}; \
         git data may not have been pushed for the force-push rebase update",
        s.grasp_update_ref_oid, s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 7: a plain `git push origin pr/feature` (without `-f`) must be
/// rejected by the remote after the rebase.
///
/// After rebasing, the local `pr/feature` history has diverged from the
/// remote tracking ref — the original PR tip is no longer an ancestor.  The
/// remote (GRASP / git-remote-nostr) must refuse a non-fast-forward push
/// unless the force flag is supplied.  If the plain push had been accepted the
/// test marks it as a failure and (in [`capture_snapshot`]) skips the
/// subsequent force push so the remaining assertions are still observable.
#[rstest]
#[tokio::test]
async fn non_force_push_rejected(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert!(
        s.non_force_push_was_rejected,
        "plain `git push origin pr/{BRANCH}` (no -f) after rebase was unexpectedly \
         accepted by the remote; a non-fast-forward push should be rejected without -f",
    );
    Ok(())
}
