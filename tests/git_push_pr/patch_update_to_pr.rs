//! Coverage for a plain (non-force) fast-forward `git push` on top of an
//! existing **patch-kind** proposal where the single new commit exceeds
//! the per-patch size threshold, triggering the patch→PR upgrade path
//! on the FF arm rather than the force-push arm.
//!
//! ## Scenario
//!
//! Mirrors [`super::patch_update`] but the single follow-up commit writes
//! a >64 KiB file, so [`Repo::are_commits_too_big_for_patches`]
//! (`src/lib/git/mod.rs:448-459`) returns true and the FF push routes
//! through the size-driven branch in
//! `src/bin/git_remote_nostr/push.rs:536-556`:
//!
//! ```text
//! if effective_root.kind.eq(&KIND_PULL_REQUEST)
//!     || git_repo.are_commits_too_big_for_patches(&ahead)
//!     || git_repo.do_commits_contain_submodules(&ahead)
//! {
//!     // emit a PR-kind event via generate_patches_or_pr_event_or_pr_updates
//! }
//! ```
//!
//! Because the existing thread root is patch-kind, the upgrade path runs
//! `generate_unsigned_pr_or_update_event` with `root_proposal =
//! Some(original_root_patch)`.  The resulting event is `KIND_PULL_REQUEST`
//! (not `KIND_PULL_REQUEST_UPDATE`) and carries the
//! `pr_specific_tags()`-with-cover-letter shape from
//! `git_events.rs:545-556` — `e` tag back-referencing the original root
//! patch, `p` tag back-referencing its author, and `branch-name` matching
//! the original series.
//!
//! Unlike [`super::patch_update_force_to_pr`] there is no amend step and
//! no `-f`, and no follow-up push — this test isolates the
//! "size-triggered upgrade on a plain FF push" path and the resulting
//! single-PR-branch advertisement on a fresh clone.
//!
//! ## Assertions (one `#[rstest]` per)
//!
//! - `two_patch_events_total` — 2 original patches, no new patch events from
//!   the FF push (the upgrade emits a PR instead of a third patch).
//! - `one_pr_event` — exactly one `KIND_PULL_REQUEST` event on the GRASP.
//! - `zero_pr_update_events` — the upgrade emits `KIND_PULL_REQUEST`, not
//!   `KIND_PULL_REQUEST_UPDATE`.
//! - `pr_event_e_tag_references_original_root` — PR event carries an `e` tag
//!   with the original root patch id.
//! - `pr_event_p_tag_references_original_root_author` — PR event carries a `p`
//!   tag with the original root patch author's pubkey.
//! - `pr_event_branch_name_matches_original_series` — PR event's `branch-name`
//!   tag equals the original series branch name.
//! - `pr_event_c_tag_is_new_commit` — PR event's `c` tag equals the new commit
//!   OID.
//! - `pr_event_authored_by_maintainer` — PR event is signed by the maintainer
//!   (the actor who ran the push).
//!
//! Fresh-clone `git ls-remote`:
//!
//! - `fresh_clone_exactly_one_pr_branch` — exactly one branch-shaped
//!   advertisement.
//! - `fresh_clone_pr_branch_uses_original_root_shorthand_and_latest_tip` — the
//!   advertised ref ends in `pr/<branch>(<original_root_8>)` and resolves to
//!   the new commit OID.

use std::{collections::BTreeMap, sync::Arc};

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
const IDENTIFIER: &str = "git-push-pr-patch-update-to-pr";

/// Size of the new file's body, in bytes.  Must exceed the per-patch
/// threshold of `(65 - 1) * 1024 = 65536` bytes enforced by
/// `Repo::are_commits_too_big_for_patches` (`src/lib/git/mod.rs:448-459`).
/// 100 KiB gives comfortable headroom even after patch-header overhead.
const BIG_FILE_BYTES: usize = 100 * 1024;

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Everything observable after the publish → clone → big-commit → push
/// arrangement, captured once per test binary via [`SNAPSHOT`] and shared
/// read-only across all `#[rstest]` cases.
struct Snapshot {
    /// All `Kind::GitPatch` events on the GRASP.  Expected count: 2 —
    /// the two original `publish_patch_series` events.  The size-triggered
    /// FF push emits a PR-kind event in their place, not a third patch.
    all_patch_events: Vec<Event>,

    /// All `KIND_PULL_REQUEST` events on the GRASP.  Expected: exactly
    /// one — the size-triggered upgrade.
    pr_events: Vec<Event>,

    /// The single upgrade PR event extracted from [`Self::pr_events`] —
    /// disambiguated by `branch-name` matching the original series.
    pr_event: Event,

    /// Total `KIND_PULL_REQUEST_UPDATE` events on the GRASP.  Must be 0
    /// — the upgrade is the new root, not an update.
    pr_update_count: usize,

    /// Event ID of the original series root patch published by
    /// [`Harness::publish_patch_series`].  The upgrade PR event's `e` tag
    /// must equal this, and the fresh-clone advertised shorthand must
    /// come from this id's first 8 hex chars.
    original_root_patch_id: EventId,

    /// Pubkey of the original series root patch (the contributor).  The
    /// upgrade PR event's `p` tag must equal this.
    original_root_patch_pubkey: PublicKey,

    /// Branch name of the original series — what the upgrade PR event's
    /// `branch-name` tag must equal.
    original_branch_name: String,

    /// New commit OID — what the upgrade PR event's `c` tag must equal,
    /// and what the fresh-clone branch-shaped advertisement must
    /// resolve to.
    new_commit_oid: String,

    /// Maintainer's pubkey — the actor running the push and therefore
    /// the expected signer of the upgrade PR event.
    maintainer_pubkey: PublicKey,

    /// Refs advertised by `git ls-remote origin` against the `nostr://`
    /// URL from a third clone with no nostr login (`CloneLogin::None`).
    /// Each entry is `(ref_name, oid)`.
    nostr_clone_ls_refs: BTreeMap<String, String>,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary
/// via [`SNAPSHOT`] and hand each case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("git_push_pr::patch_update_to_pr fixture: capture_snapshot failed"),
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
            display_name: Some("patch-update-to-pr maintainer".into()),
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

    // --- 6. Checkout proposal branch -----------------------------------------
    maintainer_clone
        .git_ok(
            ["checkout", &remote_branch],
            &format!("git checkout {remote_branch}"),
        )
        .await?;

    // --- 7. Commit one big file ----------------------------------------------
    //
    // `are_commits_too_big_for_patches` (src/lib/git/mod.rs:448-459) measures
    // the size of `make_patch_from_commit` output.  A 100 KiB file produces
    // a patch well above the 64 KiB threshold, so the FF push routes through
    // the PR-emission arm of `git_remote_nostr/push.rs:536-556`.
    //
    // Use a deterministic, easily-compressible string so the file's content
    // is bounded in size and obvious in any diagnostic dump.
    let big_content: String = "x".repeat(BIG_FILE_BYTES);
    std::fs::write(
        maintainer_clone.dir().join("maintainer-big-update.md"),
        &big_content,
    )
    .context("failed to write maintainer-big-update.md")?;
    maintainer_clone
        .git_ok(
            ["add", "maintainer-big-update.md"],
            "git add maintainer-big-update.md",
        )
        .await?;
    maintainer_clone
        .git_ok(
            [
                "commit",
                "-m",
                "follow-up on patch series (big)",
                "--no-gpg-sign",
            ],
            "git commit maintainer-big-update.md",
        )
        .await?;

    let new_commit_oid = maintainer_clone
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after big-commit")?;

    // --- 8. Plain FF push (patch → PR upgrade) -------------------------------
    //
    // `nostr_push` ticks one unix second before pushing.  No `-f` prefix:
    // this is a fast-forward push.  The remote helper resolves the single
    // commit ahead of the proposal tip; `are_commits_too_big_for_patches`
    // returns true; the push fires the size-triggered upgrade arm at
    // `push.rs:536-556`.  Expected emission: one `KIND_PULL_REQUEST` event,
    // no new `Kind::GitPatch` events, no `KIND_PULL_REQUEST_UPDATE` events.
    maintainer_clone
        .nostr_push(["origin", &remote_branch])
        .await
        .context("nostr_push of big follow-up (FF) failed")?;

    // --- 9. Query GRASP state ------------------------------------------------
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

    // --- 10. Identify the upgrade PR event -----------------------------------
    let pr_event = pr_events
        .iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(series.branch_name.as_str()))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no KIND_PULL_REQUEST event on GRASP with branch-name={:?} after the \
                 FF push; all PR event ids: {:?}",
                series.branch_name,
                pr_events.iter().map(|e| e.id.to_hex()).collect::<Vec<_>>(),
            )
        })?;

    // --- 11. Fresh nostr-URL clone: run `git ls-remote` ----------------------
    //
    // No logged-in nostr identity (`CloneLogin::None`), so
    // `list.rs:236-244` keeps the long-form `pr/<branch>(<8-hex>)` ref
    // name.  The 8-hex shorthand comes from `event_to_cover_letter(
    // proposal)` where `proposal` is the original patch root
    // (revision-root events are filtered out by `utils.rs:144-149`).
    // The OID resolves to the latest PR-or-PR-update's `c` tag — here,
    // the single upgrade PR event's `c` (= the new commit OID).
    let new_clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    let ls_out = new_clone
        .git(["ls-remote", "origin"])
        .output()
        .await
        .context("failed to spawn git ls-remote origin on fresh clone")?;
    anyhow::ensure!(
        ls_out.status.success(),
        "fresh-clone `git ls-remote origin` exited {:?}\nstdout: {}\nstderr: {}",
        ls_out.status,
        String::from_utf8_lossy(&ls_out.stdout),
        String::from_utf8_lossy(&ls_out.stderr),
    );
    let ls_stdout = String::from_utf8(ls_out.stdout)
        .context("fresh-clone `git ls-remote origin` stdout is not UTF-8")?;
    let nostr_clone_ls_refs: BTreeMap<String, String> = ls_stdout
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("ref: "))
        .filter_map(|l| l.split_once('\t'))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect();

    Ok(Snapshot {
        all_patch_events,
        pr_events,
        pr_event,
        pr_update_count,
        original_root_patch_id,
        original_root_patch_pubkey,
        original_branch_name: series.branch_name.clone(),
        new_commit_oid,
        maintainer_pubkey,
        nostr_clone_ls_refs,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Exactly 2 `Kind::GitPatch` events on the GRASP: both from
/// `publish_patch_series`.  The FF push must not emit a third patch —
/// the size-triggered upgrade emits a PR-kind event instead.
///
/// A count of 3 would mean the size guard at `push.rs:536-538` failed to
/// fire and the push fell through to the per-commit patch loop at
/// `push.rs:558-579` (the [`super::patch_update`] behaviour).
#[rstest]
#[tokio::test]
async fn two_patch_events_total(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.all_patch_events.len(),
        2,
        "expected exactly 2 Kind::GitPatch events on GRASP \
         (both from publish_patch_series; FF push must emit a PR not a patch); \
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
/// upgrade that the size threshold triggered on the FF arm.
#[rstest]
#[tokio::test]
async fn one_pr_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_events.len(),
        1,
        "expected exactly one KIND_PULL_REQUEST event on GRASP after the \
         size-triggered FF push; got {} (event ids: {:?})",
        s.pr_events.len(),
        s.pr_events
            .iter()
            .map(|e| e.id.to_hex())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Zero `KIND_PULL_REQUEST_UPDATE` events — the upgrade emits a fresh
/// `KIND_PULL_REQUEST` (the root of the new PR thread), not an update.
#[rstest]
#[tokio::test]
async fn zero_pr_update_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events on GRASP after the \
         patch→PR upgrade FF push; got {}",
        s.pr_update_count,
    );
    Ok(())
}

/// Upgrade PR event carries an `e` tag pointing at the original series
/// root patch — produced by `pr_specific_tags()` (`git_events.rs:545-556`)
/// when the upgrade path finds an existing patch-kind root.
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
/// branch name (carried over from the root patch's cover letter via
/// `event_to_cover_letter`).
#[rstest]
#[tokio::test]
async fn pr_event_branch_name_matches_original_series(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "branch-name").as_deref(),
        Some(s.original_branch_name.as_str()),
        "PR event branch-name tag should be {:?}; got {:?}",
        s.original_branch_name,
        tag_value(&s.pr_event, "branch-name"),
    );
    Ok(())
}

/// Upgrade PR event's `c` tag equals the new commit OID — what the FF
/// push actually published.
#[rstest]
#[tokio::test]
async fn pr_event_c_tag_is_new_commit(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "c").as_deref(),
        Some(s.new_commit_oid.as_str()),
        "PR event c tag should equal new commit OID {:?}; got {:?}",
        s.new_commit_oid,
        tag_value(&s.pr_event, "c"),
    );
    Ok(())
}

/// Upgrade PR event is signed by the maintainer — the actor running the
/// push owns the upgrade, not the original series author.
#[rstest]
#[tokio::test]
async fn pr_event_authored_by_maintainer(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_event.pubkey,
        s.maintainer_pubkey,
        "upgrade PR event should be authored by the maintainer ({}) who ran \
         the push; got {}",
        s.maintainer_pubkey.to_hex(),
        s.pr_event.pubkey.to_hex(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Fresh-clone `git ls-remote` — proposal branch shorthand & tip OID
// ---------------------------------------------------------------------------
//
// `list.rs` exposes two advertisement namespaces:
//
// * `refs/pr/<branch>(<shorthand>)` and `refs/pr/<event-id>/head` — emitted by
//   `get_all_proposals_state` (`list.rs:300-334`) for every proposal.
// * `refs/heads/pr/<branch>(<shorthand>)` — emitted by
//   `get_open_and_draft_proposals_state` (`list.rs:143-296`) for open/draft
//   proposals once the tip commit is locally available.
//
// We assert on the branch-shaped advertisement that ends in
// `pr/<branch>(<shorthand>)` without pinning to either prefix.

/// Helper: collect every ref whose name ends in
/// `pr/<branch>(<8-hex>)` from the fresh-clone `ls-remote` map.  Excludes
/// the `refs/pr/<full-event-id>/head` canonical refs.
fn branch_shaped_pr_refs<'a>(
    map: &'a std::collections::BTreeMap<String, String>,
    branch: &str,
) -> Vec<(&'a String, &'a String)> {
    let needle_prefix = format!("pr/{branch}(");
    map.iter()
        .filter(|(name, _)| {
            let suffix = name
                .strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/"))
                .unwrap_or(name.as_str());
            suffix.starts_with(&needle_prefix) && suffix.ends_with(')')
        })
        .collect()
}

/// Exactly two branch-shaped proposal advertisements in the fresh clone's
/// `ls-remote` output — one per namespace:
///
/// * `refs/pr/<branch>(<shorthand>)` from `get_all_proposals_state`
/// * `refs/heads/pr/<branch>(<shorthand>)` from
///   `get_open_and_draft_proposals_state`
///
/// Four would mean `list.rs` is double-advertising the proposal — for
/// example, by treating the PR upgrade event as its own proposal root
/// (which would happen if revision-root filtering in
/// `utils.rs:144-149` stopped masking PR-kind revisions).
#[rstest]
#[tokio::test]
async fn fresh_clone_exactly_one_pr_branch(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let pr_branch_refs = branch_shaped_pr_refs(&s.nostr_clone_ls_refs, &s.original_branch_name);
    assert_eq!(
        pr_branch_refs.len(),
        2,
        "expected exactly two branch-shaped proposal advertisements \
         (one each under `refs/pr/` and `refs/heads/pr/` for `{}`) in the \
         fresh clone; got {} (full ls-remote map: {:#?})",
        s.original_branch_name,
        pr_branch_refs.len(),
        s.nostr_clone_ls_refs,
    );
    Ok(())
}

/// Each branch-shaped advertisement uses the **original root patch
/// id**'s 8-char shorthand and resolves to the new commit OID.
///
/// Branch-name shorthand comes from `event_to_cover_letter(proposal)`
/// (`list.rs:234-235` / `list.rs:308-309`) where `proposal` is filtered
/// to exclude revision-root / PR-upgrade events at `utils.rs:144-149`.
/// Tip OID comes from the latest PR-or-PR-update's `c` tag
/// (`list.rs:247-258` / `list.rs:319-325`) — here, the single upgrade
/// PR event.
#[rstest]
#[tokio::test]
async fn fresh_clone_pr_branch_uses_original_root_shorthand_and_latest_tip(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let pr_branch_refs = branch_shaped_pr_refs(&s.nostr_clone_ls_refs, &s.original_branch_name);
    let (ref_name, oid) = pr_branch_refs.first().ok_or_else(|| {
        anyhow!(
            "no branch-shaped proposal advertisement in fresh clone ls-remote; \
             full map: {:#?}",
            s.nostr_clone_ls_refs,
        )
    })?;
    let original_shorthand = &s.original_root_patch_id.to_hex()[..8];
    let expected_suffix = format!("pr/{}({})", s.original_branch_name, original_shorthand);
    assert!(
        ref_name.ends_with(&expected_suffix),
        "branch-shaped proposal advertisement {:?} should end in {:?} (using \
         the original root patch id shorthand, not the PR upgrade event \
         shorthand); full ls-remote map: {:#?}",
        ref_name,
        expected_suffix,
        s.nostr_clone_ls_refs,
    );
    assert_eq!(
        oid.as_str(),
        s.new_commit_oid.as_str(),
        "branch-shaped proposal advertisement {:?} should resolve to the \
         new commit OID {:?} (read from the upgrade PR event's `c` tag); \
         got {:?}",
        ref_name,
        s.new_commit_oid,
        oid,
    );
    Ok(())
}
