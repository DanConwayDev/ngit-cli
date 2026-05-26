//! End-to-end coverage of a fast-forward `git push origin pr/feature` on top
//! of an existing KIND_PULL_REQUEST event (push.rs:508-575) — the
//! merge-base preservation introduced in commit 840c581.
//!
//! ## What push.rs:666-674 changed (840c581 regression target)
//!
//! `generate_patches_or_pr_event_or_pr_updates`
//! (`src/bin/git_remote_nostr/push.rs`) reads the `merge-base` tag off the
//! **existing** PR event rather than computing
//! `get_commit_parent(first_new_commit)`. Using the parent of the first *new*
//! commit would set the merge-base to the previous PR tip — incorrect.  The
//! original merge-base (where the branch diverged from main) lives in the root
//! PR's `merge-base` tag and must be forwarded verbatim to the update event.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`) + one GRASP server (`"repo"`).
//! 2. Maintainer publishes the repo via [`Harness::publish_repo`].
//! 3. Fresh contributor clones and logs in.
//! 4. Contributor checks out `pr/feature`, makes two commits (`t1.md`,
//!    `t2.md`).  The parent of the first commit is `published.initial_oid` —
//!    that will be `merge_base_oid`.
//! 5. **Maintainer advances `main`** so that `merge_base_oid ≠ current main
//!    tip` — makes the merge-base assertion non-trivial.
//! 6. Contributor runs `git push -u origin pr/feature` (first push).  This
//!    creates the **original KIND_PULL_REQUEST** event; its `merge-base` tag
//!    equals `merge_base_oid` (= `published.initial_oid`).
//! 7. Contributor commits one more file (`t3.md`) on the same branch.
//! 8. Contributor runs `git push origin pr/feature` (second push, no `-f`, no
//!    `-u`).  This is the fast-forward update under test; it must produce
//!    exactly one KIND_PULL_REQUEST_UPDATE and no new KIND_PULL_REQUEST.
//! 9. [`capture_snapshot`] reads all observable side-effects into a
//!    [`Snapshot`]; the harness drops. Each `#[rstest]` case asserts on one
//!    slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. **one_pr_one_update** — exactly one KIND_PULL_REQUEST and one
//!    KIND_PULL_REQUEST_UPDATE exist; the second push did not emit a new PR.
//! 2. **update_E_tag_points_at_original_pr** — the update event's uppercase `E`
//!    tag equals the original PR event ID.
//! 3. **update_merge_base_equals_original_merge_base** ← 840c581 regression
//!    catcher — the update's `merge-base` tag equals the original PR's
//!    `merge-base` tag, not the parent of the first *new* commit.
//! 4. **update_c_tag_is_new_tip** — the update event's `c` tag equals the tip
//!    OID after committing `t3.md`.
//! 5. **publisher_pr_remote_tracking_advanced_to_new_tip** — the contributor's
//!    `refs/remotes/origin/pr/feature` resolved to the new tip.
//! 6. **grasp_has_refs_nostr_for_update** — the GRASP bare repo carries
//!    `refs/nostr/<update_event_id>` resolving to the new tip OID.

use std::sync::Arc;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for the test repo — distinct from `new_pr` to avoid
/// cross-test relay pollution on the shared vanilla relay.
const IDENTIFIER: &str = "git-push-pr-ff-update";

/// Feature branch name; pushed as `pr/feature`.
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot — all observable side-effects captured once and shared
// ---------------------------------------------------------------------------

/// Everything observable after the two-push arrangement (original PR, then
/// fast-forward update), captured during [`capture_snapshot`] and shared
/// read-only across the six `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST_UPDATE event produced by the second push.
    /// Assertions 2–4 read tag values from here.
    pr_update_event: Event,

    /// Total KIND_PULL_REQUEST events authored by the contributor on the GRASP
    /// after both pushes.  Must equal 1 (assertion 1 — the second push must
    /// not have emitted a new PR).
    pr_count: usize,

    /// Total KIND_PULL_REQUEST_UPDATE events authored by the contributor on
    /// the GRASP after both pushes.  Must equal 1 (assertion 1).
    pr_update_count: usize,

    /// Hex-encoded event ID of the original KIND_PULL_REQUEST event.  The
    /// update's uppercase `E` tag must equal this (assertion 2).
    original_pr_event_id: String,

    /// The `merge-base` tag value of the original KIND_PULL_REQUEST event —
    /// equals `published.initial_oid`.  The update must carry the same value
    /// (assertion 3 — 840c581 regression catcher).
    original_merge_base: String,

    /// Contributor's `pr/feature` tip OID after committing `t3.md` (the
    /// third commit, before the second push).  The update's `c` tag and the
    /// remote-tracking ref must both equal this (assertions 4 & 5).
    update_tip_oid: String,

    /// `refs/remotes/origin/pr/feature` from the contributor's repo snapshot
    /// taken after the second push.  Must equal `update_tip_oid` (assertion 5).
    contributor_remote_tracking_oid: String,

    /// OID that `refs/nostr/<update_event_id>` resolves to in the GRASP bare
    /// repo.  Must equal `update_tip_oid` (assertion 6).
    grasp_update_ref_oid: String,
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
                    .expect("git_push_pr::ff_update fixture: capture_snapshot failed"),
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
            display_name: Some("ff-update maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    // --- 3. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "ff-update contributor".into(),
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
    // merge_base_oid that must survive into both the original PR and the update.
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

    // --- 5. Maintainer: advance main -----------------------------------------
    //
    // This creates a gap between `merge_base_oid` (= initial_oid, the fork
    // point) and the current `main` tip.  Without this step, the merge-base
    // assertion would be trivially satisfied even by a broken implementation
    // that uses `main` tip as the merge-base.
    std::fs::write(publisher.dir().join("t-on-main.md"), "content\n")
        .context("failed to write t-on-main.md on publisher side")?;
    publisher
        .git_ok(["add", "t-on-main.md"], "git add t-on-main.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main", "--no-gpg-sign"],
            "git commit advance main",
        )
        .await?;
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("maintainer nostr_push to advance main failed")?;

    // --- 6. Contributor: first push — creates original KIND_PULL_REQUEST -----
    //
    // `nostr_push` ticks one whole unix second before pushing to avoid
    // created_at collisions with the publish_repo state event.  `-u` sets
    // upstream tracking; the second push does NOT use `-u` again (plain FF).
    contributor
        .nostr_push(["-u", "origin", &format!("pr/{BRANCH}")])
        .await
        .context("first nostr_push -u origin pr/feature failed")?;

    // Capture the original PR event from the GRASP so we have its ID and
    // merge-base tag.
    let pr_events_after_first_push = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let original_pr_event = pr_events_after_first_push
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on GRASP after first `git push -u origin pr/feature`",
        )?;
    let original_pr_event_id = original_pr_event.id.to_hex();
    let original_merge_base = tag_value(&original_pr_event, "merge-base").with_context(|| {
        format!(
            "original PR event (id={original_pr_event_id}) has no `merge-base` tag; \
             cannot verify 840c581 behaviour"
        )
    })?;

    // --- 7. Contributor: third commit (t3.md) — the FF update commit ---------
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

    // --- 8. Contributor: second push — fast-forward update -------------------
    //
    // Plain `git push origin pr/feature` (no -f, no -u).  The remote helper
    // detects that a KIND_PULL_REQUEST already exists for this branch (via
    // `get_all_proposals_by_contributor` + branch-name matching), determines
    // the pushed branch is ahead by one commit (no commits behind), and
    // routes through `generate_patches_or_pr_event_or_pr_updates` with
    // `root_proposal = Some(original_pr_event)`, producing kind 1619.
    contributor
        .nostr_push(["origin", &format!("pr/{BRANCH}")])
        .await
        .context("second nostr_push origin pr/feature failed")?;

    // --- 9. Capture contributor local state after second push ----------------
    let contributor_snap = contributor
        .snapshot()
        .context("capturing contributor snapshot after second push")?;
    let remote_tracking_ref = format!("refs/remotes/origin/pr/{BRANCH}");
    let contributor_remote_tracking_oid = contributor_snap
        .refs
        .get(&remote_tracking_ref)
        .with_context(|| {
            format!(
                "{remote_tracking_ref} missing from contributor refs after second push — \
                 update_remote_refs_pushed (push.rs:165-170) did not run"
            )
        })?
        .clone();

    // --- 10. Capture events from the GRASP after second push -----------------
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
         after second `git push origin pr/feature`",
    )?;

    // --- 11. Read the GRASP bare-repo ref before the harness drops -----------
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
        original_merge_base,
        update_tip_oid,
        contributor_remote_tracking_oid,
        grasp_update_ref_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST and one KIND_PULL_REQUEST_UPDATE
/// exist on the GRASP after both pushes.
///
/// A PR count > 1 means the second push published a new PR instead of an
/// update.  An update count > 1 indicates a duplicate-publish bug.
#[rstest]
#[tokio::test]
async fn one_pr_one_update(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 1,
        "expected exactly one KIND_PULL_REQUEST on GRASP after both pushes; \
         got {} — did the second push produce a new PR instead of an update?",
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
/// The uppercase `E` tag is written by `pr_update_specific_tags`
/// (git_events.rs:527-529) using the root proposal's event ID.  It is how
/// review tools correlate the update back to the original PR thread.  A
/// missing or wrong `E` tag would silently orphan the update.
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

/// Assertion 3 (840c581 regression catcher): the update event's `merge-base`
/// tag equals the **original** PR's `merge-base` tag.
///
/// The 840c581 fix (push.rs:666-674) reads the merge-base from the root PR
/// event's `merge-base` tag.  Before the fix, the code recomputed it as the
/// parent of the first *new* commit — i.e., the previous PR tip — which is
/// wrong.  Because the maintainer advanced main in step 5, the current main
/// tip ≠ `original_merge_base`, so a regression that substitutes the
/// parent-of-first-new-commit would produce a different, incorrect value.
#[rstest]
#[tokio::test]
async fn update_merge_base_equals_original_merge_base(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "merge-base").as_deref(),
        Some(s.original_merge_base.as_str()),
        "update event `merge-base` tag should equal original PR's merge-base \
         (840c581 regression catcher); got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "merge-base"),
        s.original_merge_base,
    );
    Ok(())
}

/// Assertion 4: the update event's `c` tag equals the new tip OID (the
/// `t3.md` commit).
///
/// The `c` tag is what `get_commit_id_from_patch` (git_events.rs:58-60)
/// reads to locate the tip commit for checkout / apply operations.  An
/// incorrect value would silently produce the wrong working tree.
#[rstest]
#[tokio::test]
async fn update_c_tag_is_new_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "c").as_deref(),
        Some(s.update_tip_oid.as_str()),
        "update event `c` tag should equal the new tip OID (t3.md commit); \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "c"),
        s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 5: the contributor's `refs/remotes/origin/pr/feature` points at
/// the new tip OID after the second push.
///
/// `update_remote_refs_pushed` (push.rs:165-170) must run for both the first
/// push and the FF update push. This assertion verifies that the second push
/// advanced the remote-tracking ref rather than leaving it at the first
/// push's tip.
#[rstest]
#[tokio::test]
async fn publisher_pr_remote_tracking_advanced_to_new_tip(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.contributor_remote_tracking_oid, s.update_tip_oid,
        "contributor refs/remotes/origin/pr/{BRANCH} ({}) should point at new tip ({}); \
         was the remote-tracking ref advanced by the second push?",
        s.contributor_remote_tracking_oid, s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 6: the GRASP bare repo has `refs/nostr/<update_event_id>`
/// resolving to the new tip OID.
///
/// Proof that the git data for the FF update actually landed on the GRASP
/// server — not just the kind-1619 nostr event. Without this ref the updated
/// commits cannot be fetched from the GRASP URL.
#[rstest]
#[tokio::test]
async fn grasp_has_refs_nostr_for_update(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_update_ref_oid, s.update_tip_oid,
        "GRASP refs/nostr/<update_event_id> resolves to {} but expected update tip {}; \
         git data may not have been pushed for the FF update",
        s.grasp_update_ref_oid, s.update_tip_oid,
    );
    Ok(())
}
