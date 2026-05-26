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
//! Force-push (patch→PR upgrade):
//!
//! - `three_patch_events_total` — 2 original + 1 FF push, no new patches from
//!   the force push (the upgrade replaces patches with a single PR)
//! - `one_pr_event` — exactly one `KIND_PULL_REQUEST` event on the GRASP
//! - `zero_pr_update_events_after_upgrade` — patch→PR upgrade emits
//!   `KIND_PULL_REQUEST`, not `KIND_PULL_REQUEST_UPDATE`
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
//!
//! Follow-up push (PR update against the upgraded thread):
//!
//! - `pr_update_event_exists` — exactly one `KIND_PULL_REQUEST_UPDATE` event
//!   after the follow-up push
//! - `no_new_patch_events_from_followup` — patch count stays at 3
//! - `still_only_one_pr_event_after_followup` — PR count stays at 1
//! - `pr_update_event_e_tag_references_pr_event` — **load-bearing for
//!   e66a47a**.  The update's `E` tag must reference the upgrade PR event id,
//!   proving that `effective_root = pr_upgrade_root.unwrap_or(proposal)`
//!   (push.rs:475-478) threaded the PR event through.
//! - `pr_update_event_e_tag_does_not_reference_original_patch_root` — negative
//!   companion: the update must NOT `E`-tag the pre-upgrade patch root.  Locks
//!   out a regression to the old behaviour.
//! - `pr_update_event_p_tag_references_pr_event_author` — the `P` tag matches
//!   the PR event's author (the maintainer).
//! - `pr_update_event_c_tag_is_followup_tip` — the update's `c` tag equals the
//!   follow-up commit OID.
//! - `pr_update_event_authored_by_maintainer` — the update is signed by the
//!   maintainer.
//!
//! Fresh-clone `git ls-remote` (proposal branch advertisement):
//!
//! `list.rs` uses two ref namespaces: `refs/pr/<branch>(<shorthand>)` (every
//! proposal, regardless of status) and `refs/heads/pr/<branch>(<shorthand>)`
//! (open/draft proposals whose tip is locally available — the user-facing
//! remote-branch surface).  We assert on whichever namespace the proposal
//! lands in by matching refs that end in `pr/<branch>(<8-hex>)`.
//!
//! - `fresh_clone_exactly_one_pr_branch` — exactly one branch-shaped
//!   advertisement.
//! - `fresh_clone_pr_branch_uses_original_root_shorthand_and_latest_tip` — the
//!   advertised ref ends in `pr/<branch>(<original_root_8>)` and resolves to
//!   the follow-up tip OID.  Shorthand sourced from the original patch root id
//!   (revision-root events excluded from `proposal` by `utils.rs:144-149`); OID
//!   from the latest PR-update's `c` tag.
//! - `fresh_clone_does_not_advertise_pr_event_shorthand_branch` — negative
//!   companion: no `pr/<branch>(<pr_event_8>)` advertisement in any namespace.

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
/// force-push → follow-up-push arrangement, captured once per test binary
/// via [`SNAPSHOT`] and shared read-only across all `#[rstest]` cases.
///
/// State captured at two distinct stages of the scenario:
///
/// * "stage 1" (immediately after the force push that does the patch→PR
///   upgrade) — only one field, [`Self::pr_update_count_after_upgrade`], is
///   read here, locking in that the upgrade itself does **not** emit a
///   PR-update event.
///
/// * "final" (after the subsequent fast-forward push that adds a new commit on
///   top of the upgraded PR) — every other field is the final GRASP state.
///   Patch counts and PR counts are unchanged from stage 1 because the
///   follow-up push goes through the PR-update path.
struct Snapshot {
    /// All `Kind::GitPatch` events on the GRASP at the end of the scenario.
    /// Expected count: 3 (2 original + 1 first push).  Neither the force
    /// push nor the follow-up push emits a new patch event.
    all_patch_events: Vec<Event>,

    /// All `KIND_PULL_REQUEST` events on the GRASP at the end of the
    /// scenario.  Expected: exactly one — the patch→PR upgrade event.
    /// The follow-up push must not emit a second PR event.
    pr_events: Vec<Event>,

    /// The single upgrade PR event extracted from [`Self::pr_events`] —
    /// disambiguated by `branch-name` matching the original series.
    pr_event: Event,

    /// Count of `KIND_PULL_REQUEST_UPDATE` events on the GRASP captured
    /// immediately after the force push and before the follow-up push.
    /// Must be 0 — the upgrade is itself the new root, not an update.
    pr_update_count_after_upgrade: usize,

    /// The single `KIND_PULL_REQUEST_UPDATE` event published by the
    /// follow-up push.  Its `E` tag must reference the upgrade PR event
    /// (not the original patch root), confirming `effective_root =
    /// pr_upgrade_root.unwrap_or(proposal)` in
    /// `git_remote_nostr/push.rs:469-478` correctly threaded the PR event
    /// through.
    pr_update_event: Event,

    /// Total `KIND_PULL_REQUEST_UPDATE` events on the GRASP at the end of
    /// the scenario.  Must be 1.
    pr_update_count_final: usize,

    /// Event ID of the original series root patch published by
    /// [`Harness::publish_patch_series`].  The upgrade PR event's `e` tag
    /// must equal this.  Critically, the **PR-update event's `E` tag
    /// must NOT equal this** — that's the regression the e66a47a fix
    /// prevents.
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

    /// Tip OID after the follow-up commit — what the PR-update event's
    /// `c` tag must equal.
    followup_tip_oid: String,

    /// Maintainer's pubkey — the actor running both the force push and
    /// the follow-up push, and therefore the expected signer of the
    /// upgrade PR event and the PR-update event.
    maintainer_pubkey: PublicKey,

    /// Refs advertised by a fresh `git ls-remote origin` against the
    /// `nostr://` URL from a third clone with no nostr login
    /// (`CloneLogin::None`).  Each entry is `(ref_name, oid)`.
    ///
    /// `list.rs:233-292` derives the advertised PR branch name from
    /// `event_to_cover_letter(proposal)` — and `proposal` is always the
    /// original patch root (revision-root events are filtered out by
    /// `get_open_or_draft_proposals` at `utils.rs:144-149`).  So the
    /// 8-char shorthand must come from the **original root patch id**,
    /// not the PR upgrade event id.  The advertised OID is read from the
    /// `c` tag of the latest PR-or-PR-update in the chain
    /// (`list.rs:247-258`) — i.e. the follow-up tip.
    nostr_clone_ls_refs: BTreeMap<String, String>,
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

    // --- 10. Stage-1 query: confirm no PR-update yet -------------------------
    //
    // The force-push upgrade itself must not emit a `KIND_PULL_REQUEST_UPDATE`
    // event.  We snapshot the count before doing the follow-up push so the
    // assertion can isolate "the upgrade is a fresh PR root, not an update".
    let pr_update_count_after_upgrade = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST_UPDATE))
        .await?
        .len();

    // --- 11. Follow-up commit and fast-forward push (PR update) --------------
    //
    // After the upgrade the proposal thread root is the PR event.  A
    // subsequent push on the same branch should route through the
    // patch→PR-already-PR arm of `git_remote_nostr/push.rs:533-552`
    // (`effective_root.kind == KIND_PULL_REQUEST`) and emit a
    // `KIND_PULL_REQUEST_UPDATE` event.  The fix in e66a47a ensures the
    // update's `E` tag references the PR event id (via
    // `pr_upgrade_root.unwrap_or(proposal)`), not the original patch root.
    std::fs::write(
        maintainer_clone.dir().join("followup.md"),
        "second maintainer follow-up after the PR upgrade\n",
    )
    .context("failed to write followup.md")?;
    maintainer_clone
        .git_ok(["add", "followup.md"], "git add followup.md")
        .await?;
    maintainer_clone
        .git_ok(
            [
                "commit",
                "-m",
                "follow-up after PR upgrade",
                "--no-gpg-sign",
            ],
            "git commit followup.md",
        )
        .await?;

    let followup_tip_oid = maintainer_clone
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after follow-up commit")?;

    maintainer_clone
        .nostr_push(["origin", &remote_branch])
        .await
        .context("nostr_push of follow-up commit (post-upgrade) failed")?;

    // --- 12. Final query of GRASP state --------------------------------------
    //
    // Patch and PR-event counts must be unchanged from stage 1 — the
    // follow-up push goes through the PR-update path so it produces a single
    // `KIND_PULL_REQUEST_UPDATE` event and nothing else.
    let all_patch_events = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitPatch))
        .await?;

    let pr_events = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST))
        .await?;

    let pr_update_events = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST_UPDATE))
        .await?;
    let pr_update_count_final = pr_update_events.len();

    // --- 13. Identify the upgrade PR event -----------------------------------
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

    // --- 14. Identify the PR-update event ------------------------------------
    //
    // The follow-up push must have produced exactly one
    // `KIND_PULL_REQUEST_UPDATE` event authored by the maintainer.  We
    // disambiguate by the `c` tag matching the follow-up tip OID — the
    // unique observable side-effect of the second push.
    let pr_update_event = pr_update_events
        .iter()
        .find(|e| tag_value(e, "c").as_deref() == Some(followup_tip_oid.as_str()))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no KIND_PULL_REQUEST_UPDATE event on GRASP with c={:?} after the \
                 follow-up push; all update event ids: {:?}",
                followup_tip_oid,
                pr_update_events
                    .iter()
                    .map(|e| e.id.to_hex())
                    .collect::<Vec<_>>(),
            )
        })?;

    // --- 15. Fresh nostr-URL clone: run `git ls-remote` ----------------------
    //
    // Mounts the cover-letter / list.rs round-trip end-to-end: the
    // newly-cloned repo has no logged-in nostr identity (`CloneLogin::None`),
    // so `list.rs:236-244` keeps the long-form `pr/<branch>(<8-hex>)` ref
    // name (the short form is only used when the current user is the
    // proposal author).  The 8-hex shorthand is sourced from
    // `event_to_cover_letter(proposal).event_id`, and the `proposal` for
    // an upgraded patch thread is still the **original patch root** —
    // revision-root events (including the PR upgrade) are filtered out
    // upstream by `utils.rs:144-149`.  So the asserted shorthand below
    // must equal `original_root_patch_id[..8]`, never the PR event's
    // shorthand.
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
        pr_update_count_after_upgrade,
        pr_update_event,
        pr_update_count_final,
        original_root_patch_id,
        original_root_patch_pubkey,
        original_branch_name: series.branch_name.clone(),
        amended_tip_oid,
        followup_tip_oid,
        maintainer_pubkey,
        nostr_clone_ls_refs,
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

/// Zero `KIND_PULL_REQUEST_UPDATE` events immediately after the force-
/// push upgrade — the upgrade emits a fresh `KIND_PULL_REQUEST` (the
/// root of the new PR thread), not an update.  Asserts on the stage-1
/// snapshot taken **before** the follow-up push (which does legitimately
/// emit one update event; see [`pr_update_event_exists`]).
#[rstest]
#[tokio::test]
async fn zero_pr_update_events_after_upgrade(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count_after_upgrade, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events on GRASP immediately \
         after the patch→PR upgrade force push; got {}",
        s.pr_update_count_after_upgrade,
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

// ---------------------------------------------------------------------------
// Follow-up-push assertions — PR-update threading after the upgrade
// ---------------------------------------------------------------------------

/// Exactly one `KIND_PULL_REQUEST_UPDATE` event on the GRASP at the end
/// of the scenario — the follow-up push emits one update event, not
/// multiple and not zero.
#[rstest]
#[tokio::test]
async fn pr_update_event_exists(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count_final, 1,
        "expected exactly one KIND_PULL_REQUEST_UPDATE event on GRASP after \
         the follow-up push; got {}",
        s.pr_update_count_final,
    );
    Ok(())
}

/// Follow-up push must not have produced any **additional** patch events
/// — the total stays at 3 because the existing PR-kind thread root
/// forces the push through the PR-update arm.
#[rstest]
#[tokio::test]
async fn no_new_patch_events_from_followup(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.all_patch_events.len(),
        3,
        "expected exactly 3 Kind::GitPatch events on GRASP after the follow-up \
         push (no new patches; the follow-up should be a PR-update, not a \
         patch); got {} (event ids: {:?})",
        s.all_patch_events.len(),
        s.all_patch_events
            .iter()
            .map(|e| e.id.to_hex())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Follow-up push must not have produced a second PR-kind event — the
/// upgrade PR remains the unique thread root.
#[rstest]
#[tokio::test]
async fn still_only_one_pr_event_after_followup(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_events.len(),
        1,
        "expected exactly one KIND_PULL_REQUEST event on GRASP after the \
         follow-up push (the upgrade PR is the unique thread root); got {} \
         (event ids: {:?})",
        s.pr_events.len(),
        s.pr_events
            .iter()
            .map(|e| e.id.to_hex())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// **Load-bearing for e66a47a.**  The PR-update event's `E` (uppercase,
/// root marker) tag must reference the upgrade PR event id — not the
/// original patch root.  Before the fix, `effective_root` was set to the
/// proposal directly, which for the patch→PR upgrade scenario kept
/// pointing at the original patch root and produced an `E` tag
/// referencing the wrong event.  Now `effective_root =
/// pr_upgrade_root.unwrap_or(proposal)` (push.rs:475-478) threads the PR
/// event through.
///
/// `pr_update_specific_tags` (`git_events.rs:520-535`) emits the tag as
/// `["E", <hex>]` (slot 1 carries the id).
#[rstest]
#[tokio::test]
async fn pr_update_event_e_tag_references_pr_event(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let e_values: Vec<String> = s
        .pr_update_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("E"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();
    assert!(
        e_values.contains(&s.pr_event.id.to_hex()),
        "PR-update event should carry an `E` tag with the upgrade PR event id \
         {:?}; got E values: {:?}",
        s.pr_event.id.to_hex(),
        e_values,
    );
    Ok(())
}

/// **Negative companion to [`pr_update_event_e_tag_references_pr_event`].**
/// The PR-update event must NOT reference the original patch root via
/// `E` — that's the exact regression e66a47a prevented.  Asserting the
/// absence makes the test fail loudly if a future change re-introduces
/// the "effective_root = proposal" shortcut.
#[rstest]
#[tokio::test]
async fn pr_update_event_e_tag_does_not_reference_original_patch_root(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let e_values: Vec<String> = s
        .pr_update_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("E"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();
    assert!(
        !e_values.contains(&s.original_root_patch_id.to_hex()),
        "PR-update event should NOT carry an `E` tag with the original patch \
         root id {:?} (it should reference the upgrade PR event, not the \
         pre-upgrade patch root); got E values: {:?}",
        s.original_root_patch_id.to_hex(),
        e_values,
    );
    Ok(())
}

/// PR-update event's `P` (uppercase, root author) tag must reference the
/// upgrade PR event's author — the maintainer.  Emitted by
/// `pr_update_specific_tags` as `["P", <hex>]`.  Same reasoning as the
/// `E` tag check: the root author must move with the root event when
/// the patch→PR upgrade happens.
#[rstest]
#[tokio::test]
async fn pr_update_event_p_tag_references_pr_event_author(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let p_values: Vec<String> = s
        .pr_update_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("P"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();
    assert!(
        p_values.contains(&s.pr_event.pubkey.to_hex()),
        "PR-update event should carry a `P` tag with the upgrade PR event \
         author {:?}; got P values: {:?}",
        s.pr_event.pubkey.to_hex(),
        p_values,
    );
    Ok(())
}

/// PR-update event's `c` tag equals the follow-up commit's OID — what
/// the follow-up push actually published.
#[rstest]
#[tokio::test]
async fn pr_update_event_c_tag_is_followup_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "c").as_deref(),
        Some(s.followup_tip_oid.as_str()),
        "PR-update event c tag should equal the follow-up tip OID {:?}; got {:?}",
        s.followup_tip_oid,
        tag_value(&s.pr_update_event, "c"),
    );
    Ok(())
}

/// PR-update event is signed by the maintainer — the actor running the
/// follow-up push.
#[rstest]
#[tokio::test]
async fn pr_update_event_authored_by_maintainer(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_event.pubkey,
        s.maintainer_pubkey,
        "PR-update event should be authored by the maintainer ({}) who ran \
         the follow-up push; got {}",
        s.maintainer_pubkey.to_hex(),
        s.pr_update_event.pubkey.to_hex(),
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
//   `get_all_proposals_state` (`list.rs:300-334`) for **every** proposal,
//   regardless of status.  No `does_commit_exist` check.
// * `refs/heads/pr/<branch>(<shorthand>)` — emitted by
//   `get_open_and_draft_proposals_state` (`list.rs:143-296`) only for
//   open/draft proposals AND only once the tip commit is locally available.
//   This namespace is the user-facing "remote branch" surface (`git branch
//   -r`).
//
// The two namespaces are intentionally distinct: `refs/pr/*` is the
// complete catalogue, `refs/heads/pr/*` is the filtered "show me
// branches I can interact with" subset.  We assert on the
// branch-shaped advertisement that ends in `pr/<branch>(<shorthand>)`
// without pinning to either prefix, so the test stays correct whether
// the proposal happens to land in the open/draft remote-tracking
// surface for this scenario or only in the all-proposals catalogue.

/// Helper: collect every ref whose name ends in
/// `pr/<branch>(<8-hex>)` — the branch-shaped proposal advertisement —
/// from the fresh-clone `ls-remote` map.  Excludes the
/// `refs/pr/<full-event-id>/head` canonical refs (those don't carry
/// the human branch name).
fn branch_shaped_pr_refs<'a>(
    map: &'a std::collections::BTreeMap<String, String>,
    branch: &str,
) -> Vec<(&'a String, &'a String)> {
    let needle_prefix = format!("pr/{branch}(");
    map.iter()
        .filter(|(name, _)| {
            // Trim any leading `refs/heads/` or `refs/` so we match against
            // the suffix `pr/<branch>(<...>)`.
            let suffix = name
                .strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/"))
                .unwrap_or(name.as_str());
            suffix.starts_with(&needle_prefix) && suffix.ends_with(')')
        })
        .collect()
}

/// Exactly one branch-shaped proposal advertisement
/// (`(refs/heads/)?pr/<branch>(<shorthand>)`) in the fresh clone's
/// `ls-remote` output.
///
/// Two would mean `list.rs` is double-advertising the proposal — for
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
        1,
        "expected exactly one branch-shaped proposal advertisement \
         (`(refs/heads/)?pr/{}(<shorthand>)`) in the fresh clone; got {} \
         (full ls-remote map: {:#?})",
        s.original_branch_name,
        pr_branch_refs.len(),
        s.nostr_clone_ls_refs,
    );
    Ok(())
}

/// The single branch-shaped advertisement uses the **original root patch
/// id**'s 8-char shorthand and resolves to the follow-up tip OID.
///
/// Branch-name shorthand comes from `event_to_cover_letter(proposal)`
/// (`list.rs:234-235` / `list.rs:308-309`) where `proposal` is filtered
/// to exclude revision-root / PR-upgrade events at `utils.rs:144-149`.
/// Tip OID comes from the latest PR-or-PR-update's `c` tag
/// (`list.rs:247-258` / `list.rs:319-325`) — here, the follow-up
/// push's PR-update event.
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
        s.followup_tip_oid.as_str(),
        "branch-shaped proposal advertisement {:?} should resolve to the \
         follow-up tip OID {:?} (read from the PR-update event's `c` tag); \
         got {:?}",
        ref_name,
        s.followup_tip_oid,
        oid,
    );
    Ok(())
}

/// **Negative companion** to
/// [`fresh_clone_pr_branch_uses_original_root_shorthand_and_latest_tip`]:
/// the fresh clone must NOT advertise any ref ending in
/// `pr/<branch>(<pr_event_8>)`.  If it did, the PR upgrade event would
/// be acting as an independent proposal root rather than a revision of
/// the original patch thread — exactly the failure mode that
/// revision-root filtering at `utils.rs:144-149` prevents.
#[rstest]
#[tokio::test]
async fn fresh_clone_does_not_advertise_pr_event_shorthand_branch(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let pr_event_shorthand = &s.pr_event.id.to_hex()[..8];
    let bad_suffix = format!("pr/{}({})", s.original_branch_name, pr_event_shorthand);
    let offending: Vec<&String> = s
        .nostr_clone_ls_refs
        .keys()
        .filter(|name| name.ends_with(&bad_suffix))
        .collect();
    assert!(
        offending.is_empty(),
        "fresh clone should NOT advertise any ref ending in {:?} — the PR \
         upgrade event must not appear as an independent proposal root.  \
         Offending refs: {:?}\nFull ls-remote map: {:#?}",
        bad_suffix,
        offending,
        s.nostr_clone_ls_refs,
    );
    Ok(())
}
