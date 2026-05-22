//! End-to-end coverage of `ngit send --in-reply-to` (PR Update path) where
//! the contributor **rebases** their feature branch onto a newer `main` between
//! the original PR send and the update send.
//!
//! ## What this test covers
//!
//! This is the "rebase" sibling of `send_pr_update.rs`.  The only structural
//! difference is that between steps 6 and 7 the maintainer advances `main` a
//! **second** time and the contributor fetches + rebases before adding the
//! update commit.  That rebase shifts the fork point from the original
//! `published.initial_oid` to the post-rebase `main` tip, so the `merge-base`
//! tag on the update event must reflect the new fork point.
//!
//! The key invariant under test (assertion 6) is:
//!
//! > After a rebase the `merge-base` tag on the PR update event equals the
//! > **new** fork point (the `main` tip at rebase time), not the original one.
//!
//! ## Arrangement
//!
//! Steps 1–6 mirror `send_pr_update.rs` exactly; steps 7–9 are the
//! rebase-specific divergence.
//!
//! 1. Harness: one vanilla relay (`"default"`), two grasp servers (`"repo"` and
//!    `"repo_secondary"`).
//! 2. Maintainer publishes repo with both grasps in the announcement.
//! 3. Fresh contributor clones and checks out the `"feature"` branch.
//! 4. Contributor commits `t3.md` (first PR commit; its parent is the original
//!    fork point = `published.initial_oid`).
//! 5. **Maintainer first advance of `main`** (same gap-creating step as
//!    `send_pr_update.rs` so `merge_base_oid ≠ main_tip_at_send_time` for the
//!    original PR).
//! 6. Contributor commits `t4.md` then runs `ngit send HEAD~2 --force-pr`. This
//!    is the **original PR** — captured for its event ID.
//! 7. **Maintainer second advance of `main`** — this is the "newer main" that
//!    the contributor will rebase onto.
//! 8. **Contributor fetches `origin` and rebases `feature` onto
//!    `origin/main`.** The post-rebase `HEAD~2` (= new main tip) becomes
//!    `rebased_merge_base_oid`.
//! 9. Contributor commits `t5.md` (the update commit, on top of the rebased
//!    branch tip).
//! 10. Contributor runs `ngit send HEAD~3 --in-reply-to <pr_event_id_hex>`.
//!     This is the **PR Update** — the act under test.
//! 11. [`capture_snapshot`] reads all events and git refs; harness drops. Each
//!     `#[rstest]` case asserts on one slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST_UPDATE event is published (primary grasp).
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's updated tip OID (`t5.md` commit).
//! 4. The uppercase `E` tag equals the original PR event's ID (hex).
//! 5. The uppercase `P` tag equals the original PR author's pubkey
//!    (contributor).
//! 6. The `merge-base` tag equals the **rebased** fork point (new main tip at
//!    rebase time), **not** the original fork point.
//! 7. Both grasp servers' bare repos contain `refs/nostr/<update_event_id>`
//!    resolving to the update tip OID.
//! 8. No new KIND_PULL_REQUEST event was published — the original PR event
//!    count on the primary grasp is still exactly one.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{CloneLogin, Harness, PublishRepoOpts};
use tokio::sync::OnceCell;

/// Identifier passed to `ngit init --identifier`. Distinct from both
/// `send_pr.rs` (`"pr-test-repo"`) and `send_pr_update.rs`
/// (`"pr-update-test-repo"`) so test runs on a shared vanilla-relay surface
/// cannot see each other's events.
const IDENTIFIER: &str = "pr-update-rebase-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

/// `KIND_PULL_REQUEST` (kind 1618). Mirrored from `src/lib/git_events.rs`.
const KIND_PULL_REQUEST: Kind = Kind::Custom(1618);

/// `KIND_PULL_REQUEST_UPDATE` (kind 1619). Mirrored for the same reason.
const KIND_PULL_REQUEST_UPDATE: Kind = Kind::Custom(1619);

// ---------------------------------------------------------------------------
// Snapshot — all observable side-effects captured once and shared
// ---------------------------------------------------------------------------

/// Everything observable after the two-step arrangement (original PR send
/// followed by rebase + PR update send), captured during [`capture_snapshot`]
/// and shared read-only across the eight `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST_UPDATE event published by the contributor,
    /// read from the primary grasp. Assertions 2–6 read from here.
    pr_update_event: Event,

    /// Number of KIND_PULL_REQUEST_UPDATE events authored by the contributor
    /// on the primary grasp. Must equal 1 (assertion 1).
    pr_update_count_primary: usize,

    /// Number of KIND_PULL_REQUEST events authored by the contributor on the
    /// primary grasp after both sends. Must still equal 1 (assertion 8).
    pr_count_primary: usize,

    /// Hex-encoded event ID of the original KIND_PULL_REQUEST event. The
    /// uppercase `E` tag on the PR update event must equal this (assertion 4).
    original_pr_event_id: String,

    /// Public key of the contributor — the author of the original PR event.
    /// The uppercase `P` tag on the PR update event must equal this
    /// (assertion 5).
    contributor_pubkey: PublicKey,

    /// OID of the contributor's feature-branch tip after committing `t5.md`
    /// on the rebased branch. The `c` tag on the PR update event must equal
    /// this (assertion 3).
    update_tip_oid: String,

    /// The **new** fork point after the rebase — the `main` tip at rebase
    /// time (i.e. the commit the maintainer pushed in the second main
    /// advance). The `merge-base` tag on the PR update event must equal this
    /// (assertion 6).
    rebased_merge_base_oid: String,

    /// The original fork point before the rebase (`published.initial_oid`).
    /// Kept so that assertion 6 can verify the precondition
    /// `rebased_merge_base_oid != original_fork_point_oid`, making the
    /// assertion non-trivial.
    original_fork_point_oid: String,

    /// Maintainer's public key. Used to verify the `a` tag (assertion 2).
    maintainer_pubkey: PublicKey,

    /// `d` tag identifier passed to `ngit init`. Used to verify the `a` tag
    /// (assertion 2).
    identifier: String,

    /// OID that `refs/nostr/<update_event_id>` resolves to inside the primary
    /// grasp's bare repo. Must equal `update_tip_oid` (assertion 7 primary).
    grasp_primary_update_ref_oid: String,

    /// OID that `refs/nostr/<update_event_id>` resolves to inside the
    /// secondary grasp's bare repo. Must equal `update_tip_oid` (assertion 7
    /// secondary).
    grasp_secondary_update_ref_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary via
/// [`SNAPSHOT`] and hand each test case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("send_pr_update_rebase fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- Harness: one vanilla relay + two grasp servers ----------------------
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_grasp_server("repo_secondary")
    .build()
    .await?;

    // --- 1. Maintainer publishes repo with both grasps -----------------------
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("pr-update-rebase maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            additional_grasp_roles: vec!["repo_secondary".into()],
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 2. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "pr-update-rebase contributor".into(),
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

    // --- 3. Contributor: feature branch + first commit (t3.md) ---------------
    //
    // The parent of this commit is `published.initial_oid` — that is the
    // original fork point that the `merge-base` tag must NOT equal after
    // the rebase.
    run_git(&contributor, &["checkout", "-b", BRANCH]).await?;
    std::fs::write(contributor.dir().join("t3.md"), "some content\n")
        .context("failed to write t3.md in contributor clone")?;
    run_git(&contributor, &["add", "t3.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t3.md", "--no-gpg-sign"],
    )
    .await?;

    let original_fork_point_oid = published.initial_oid.clone();

    // --- 4. Maintainer: first advance of main --------------------------------
    //
    // Creates a gap between `original_fork_point_oid` and the current `main`
    // tip so the original PR's merge-base assertion is non-trivial (same
    // rationale as send_pr_update.rs step 4).
    std::fs::write(publisher.dir().join("main-v1.md"), "content\n")
        .context("failed to write main-v1.md on publisher side")?;
    run_git(&publisher, &["add", "main-v1.md"]).await?;
    run_git(
        &publisher,
        &["commit", "-m", "advance main (v1)", "--no-gpg-sign"],
    )
    .await?;
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("maintainer nostr_push (first main advance) failed")?;

    // --- 5. Contributor: second commit (t4.md) --------------------------------
    std::fs::write(contributor.dir().join("t4.md"), "some content\n")
        .context("failed to write t4.md in contributor clone")?;
    run_git(&contributor, &["add", "t4.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t4.md", "--no-gpg-sign"],
    )
    .await?;

    // --- 6. Contributor: send original PR (HEAD~2) ----------------------------
    //
    // `HEAD~2` covers t3.md and t4.md; `--force-pr` keeps it kind 1618
    // regardless of patch size.
    let send_pr_out = contributor
        .ngit([
            "send",
            "HEAD~2",
            "--force-pr",
            "--title",
            "add feature",
            "--description",
            "this adds the feature",
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-pr (original PR)")?;
    if !send_pr_out.status.success() {
        bail!(
            "original ngit send --force-pr exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            send_pr_out.status,
            String::from_utf8_lossy(&send_pr_out.stdout),
            String::from_utf8_lossy(&send_pr_out.stderr),
        );
    }

    // Capture the original PR event so we have its ID for `--in-reply-to`.
    let pr_events_primary = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let original_pr_event = pr_events_primary
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on primary grasp after original `ngit send --force-pr`",
        )?;
    let original_pr_event_id = original_pr_event.id.to_hex();

    // --- 7. Maintainer: second advance of main (the "newer main") ------------
    //
    // This is the commit the contributor will rebase onto. After the rebase
    // its OID becomes the new fork point that must appear as `merge-base` in
    // the PR update event.
    std::fs::write(publisher.dir().join("main-v2.md"), "content\n")
        .context("failed to write main-v2.md on publisher side")?;
    run_git(&publisher, &["add", "main-v2.md"]).await?;
    run_git(
        &publisher,
        &["commit", "-m", "advance main (v2)", "--no-gpg-sign"],
    )
    .await?;
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("maintainer nostr_push (second main advance) failed")?;

    // --- 8. Contributor: fetch + rebase onto origin/main ---------------------
    //
    // `git fetch origin` pulls the two new main commits via the nostr remote
    // helper. `git rebase origin/main` replays t3.md and t4.md on top of the
    // new main tip (main-v2 commit). There are no conflicts: the maintainer
    // touched main-v1.md / main-v2.md while the contributor touched t3.md /
    // t4.md.
    //
    // After the rebase the topology is:
    //   [initial] → [main-v1] → [main-v2] → [rebased-t3] → [rebased-t4]
    //                                ↑
    //                         HEAD~2 = new fork point
    run_git(&contributor, &["fetch", "origin"]).await?;
    run_git(&contributor, &["rebase", "origin/main"]).await?;

    // Capture the new fork point immediately after the rebase (before adding
    // t5.md). After adding t5.md, HEAD~2 will be rebased-t3, but HEAD~3 will
    // still be main-v2 — so we capture HEAD~2 now = main-v2 OID.
    let rebased_merge_base_oid = git_rev_parse(&contributor, "HEAD~2").await?;

    // Sanity check: the rebase must have moved the fork point.
    if rebased_merge_base_oid == original_fork_point_oid {
        bail!(
            "setup invariant violated: rebased_merge_base_oid equals original_fork_point_oid \
             ({rebased_merge_base_oid}) — rebase did not change the fork point as expected"
        );
    }

    // --- 9. Contributor: third commit (t5.md) — the update commit -----------
    //
    // Added on top of the rebased branch. HEAD is now rebased-t4, so after
    // this commit:
    //   HEAD = t5, HEAD~1 = rebased-t4, HEAD~2 = rebased-t3, HEAD~3 = main-v2
    std::fs::write(contributor.dir().join("t5.md"), "more content\n")
        .context("failed to write t5.md in contributor clone")?;
    run_git(&contributor, &["add", "t5.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t5.md", "--no-gpg-sign"],
    )
    .await?;
    let update_tip_oid = git_rev_parse(&contributor, "HEAD").await?;

    // --- 10. Contributor: ngit send --in-reply-to (PR Update) ----------------
    //
    // `HEAD~3` is the new fork point (main-v2 commit) after the rebase and
    // the addition of t5.md, so this covers rebased-t3, rebased-t4, and t5.
    // The production code detects `as_pr = true` from the original kind-1618
    // event and routes to `KIND_PULL_REQUEST_UPDATE`.
    let send_update_out = contributor
        .ngit([
            "send",
            "HEAD~3",
            "--in-reply-to",
            &original_pr_event_id,
            "--title",
            "update: add t5 (after rebase)",
            "--description",
            "rebased feature branch and added t5.md",
        ])
        .output()
        .await
        .context("failed to spawn ngit send --in-reply-to (PR update after rebase)")?;
    if !send_update_out.status.success() {
        bail!(
            "ngit send --in-reply-to (rebase) exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            send_update_out.status,
            String::from_utf8_lossy(&send_update_out.stdout),
            String::from_utf8_lossy(&send_update_out.stderr),
        );
    }

    // --- 11. Capture events from all surfaces --------------------------------

    // Primary grasp: KIND_PULL_REQUEST_UPDATE events by contributor.
    let pr_update_events_primary = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_update_count_primary = pr_update_events_primary.len();
    let pr_update_event = pr_update_events_primary.into_iter().next().context(
        "no KIND_PULL_REQUEST_UPDATE authored by contributor found on primary grasp \
             after `ngit send --in-reply-to` (rebase variant)",
    )?;

    // Primary grasp: KIND_PULL_REQUEST events by contributor (must still be 1).
    let pr_count_primary = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?
        .len();

    // --- 12. Read git refs from both grasps before harness drops -------------
    let update_event_id_hex = pr_update_event.id.to_hex();
    let bare_primary = harness
        .grasp("repo")
        .git_data_path()
        .join(&published.maintainer_npub)
        .join(format!("{IDENTIFIER}.git"));
    let grasp_primary_update_ref_oid = read_nostr_ref(&bare_primary, &update_event_id_hex)
        .await
        .with_context(|| {
            format!(
                "reading refs/nostr/{update_event_id_hex} from primary grasp bare repo at {}",
                bare_primary.display()
            )
        })?;

    let bare_secondary = harness
        .grasp("repo_secondary")
        .git_data_path()
        .join(&published.maintainer_npub)
        .join(format!("{IDENTIFIER}.git"));
    let grasp_secondary_update_ref_oid = read_nostr_ref(&bare_secondary, &update_event_id_hex)
        .await
        .with_context(|| {
            format!(
                "reading refs/nostr/{update_event_id_hex} from secondary grasp bare repo at {}",
                bare_secondary.display()
            )
        })?;

    Ok(Snapshot {
        pr_update_event,
        pr_update_count_primary,
        pr_count_primary,
        original_pr_event_id,
        contributor_pubkey,
        update_tip_oid,
        rebased_merge_base_oid,
        original_fork_point_oid,
        maintainer_pubkey,
        identifier: IDENTIFIER.to_string(),
        grasp_primary_update_ref_oid,
        grasp_secondary_update_ref_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST_UPDATE event is published by
/// the contributor on the primary grasp.
///
/// A count > 1 would indicate a duplicate-publish bug or test-isolation
/// failure. A count of 0 would mean `capture_snapshot` bailed before
/// returning (the `context`-propagating `?` would have surfaced the error
/// as a fixture panic, not a soft assertion failure).
#[rstest]
#[tokio::test]
async fn pr_update_event_exactly_one(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count_primary, 1,
        "expected exactly one KIND_PULL_REQUEST_UPDATE on primary grasp authored by \
         contributor; got {}",
        s.pr_update_count_primary,
    );
    Ok(())
}

/// Assertion 2: the PR update event's `a` tag is the canonical 30617
/// coordinate pointing at the maintainer's repository announcement.
///
/// Identical to `send_pr_update.rs` assertion 2 — the rebase does not affect
/// how the event's repo coordinate is computed.
#[rstest]
#[tokio::test]
async fn a_tag_is_repo_coordinate(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let expected = format!("30617:{}:{}", s.maintainer_pubkey, s.identifier);
    let a_tags: Vec<&Tag> = s
        .pr_update_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("a"))
        .collect();
    assert!(
        a_tags
            .iter()
            .any(|t| t.as_slice().get(1).map(String::as_str) == Some(expected.as_str())),
        "expected an `a` tag with value {expected:?}; got a tags: {:?}",
        a_tags
            .iter()
            .filter_map(|t| t.as_slice().get(1).cloned())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Assertion 3: the PR update event's `c` tag equals the contributor's
/// feature-branch tip OID after adding `t5.md` on the rebased branch.
///
/// After the rebase the rebased commits have fresh OIDs, and `t5.md` is
/// committed on top of those. The `c` tag must equal the new `HEAD` OID —
/// `get_commit_id_from_patch` reads this to know where to fast-forward during
/// `ngit pr checkout`.
#[rstest]
#[tokio::test]
async fn c_tag_is_update_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "c").as_deref(),
        Some(s.update_tip_oid.as_str()),
        "PR update event `c` tag should equal contributor's updated tip OID \
         (after rebase + t5.md); got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "c"),
        s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 4: the PR update event's uppercase `E` tag equals the original
/// PR event's ID (hex).
///
/// The rebase changes the contributor's commits but not the identity of the
/// original PR event — `--in-reply-to` still targets the same kind-1618
/// event ID.
#[rstest]
#[tokio::test]
async fn e_tag_is_original_pr_id(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "E").as_deref(),
        Some(s.original_pr_event_id.as_str()),
        "PR update event uppercase `E` tag should equal the original PR event ID; \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "E"),
        s.original_pr_event_id,
    );
    Ok(())
}

/// Assertion 5: the PR update event's uppercase `P` tag equals the
/// contributor's public key (the author of the original PR event).
///
/// The rebase does not change the contributor's nostr identity.
#[rstest]
#[tokio::test]
async fn p_tag_is_original_pr_author(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let expected = s.contributor_pubkey.to_string();
    assert_eq!(
        tag_value(&s.pr_update_event, "P").as_deref(),
        Some(expected.as_str()),
        "PR update event uppercase `P` tag should equal contributor pubkey \
         (original PR author); got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "P"),
        expected,
    );
    Ok(())
}

/// Assertion 6: after a rebase onto a newer `main`, the `merge-base` tag
/// equals the **new** fork point, not the original one.
///
/// `select_servers_push_refs_and_generate_pr_or_pr_update_event` passes
/// `git_repo.get_commit_parent(first_commit)` as the merge_base. After
/// rebasing and running `ngit send HEAD~3`, the first commit in the range is
/// the **rebased** t3.md whose parent is the new main tip (`main-v2`). That
/// OID is `rebased_merge_base_oid`.
///
/// The precondition assertion (`rebased != original`) makes the test
/// non-trivial: if the rebase did nothing the two OIDs would be equal and
/// the test would only be checking a trivially-correct value.
///
/// A regression that reuses the previous `merge-base` from the original PR
/// event or that computes the fork point from the pre-rebase topology would
/// fail here.
#[rstest]
#[tokio::test]
async fn merge_base_tag_is_new_fork_point(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
        "PR update event `merge-base` tag should equal the new fork-point OID \
         (main-v2, the tip rebased onto); got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "merge-base"),
        s.rebased_merge_base_oid,
    );
    Ok(())
}

/// Assertion 7: both grasp servers received the git data push — each bare
/// repo has a `refs/nostr/<update_event_id>` ref resolving to the updated
/// tip OID.
///
/// The rebased commits are new objects; both grasps must store them.
#[rstest]
#[tokio::test]
async fn both_grasps_have_update_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_primary_update_ref_oid, s.update_tip_oid,
        "primary grasp: refs/nostr/<update_event_id> should resolve to update_tip_oid; \
         got {:?}, want {:?}",
        s.grasp_primary_update_ref_oid, s.update_tip_oid,
    );
    assert_eq!(
        s.grasp_secondary_update_ref_oid, s.update_tip_oid,
        "secondary grasp: refs/nostr/<update_event_id> should resolve to update_tip_oid; \
         got {:?}, want {:?}",
        s.grasp_secondary_update_ref_oid, s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 8: no new KIND_PULL_REQUEST event was published by the
/// contributor on the primary grasp — the original PR event count is still
/// exactly one.
///
/// Even though the commits changed (new OIDs after rebase), the PR update
/// event must remain kind 1619, not kind 1618. A regression that falls
/// through to the new-PR branch or fails to detect `as_pr = true` from the
/// `--in-reply-to` target would publish a stray kind-1618 event.
#[rstest]
#[tokio::test]
async fn no_new_pr_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count_primary, 1,
        "expected exactly one KIND_PULL_REQUEST on primary grasp after both sends; \
         got {} — did the rebase update accidentally publish a new PR?",
        s.pr_count_primary,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — identical to send_pr_update.rs copies; kept local to avoid
// cross-test coupling.
// ---------------------------------------------------------------------------

/// Run `git <args>` inside `repo`, bailing with captured output on non-zero
/// exit.
async fn run_git(repo: &test_harness::Repo, args: &[&str]) -> Result<()> {
    let label = format!("git {}", args.join(" "));
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn `{label}`"))?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "`{label}` exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }
}

/// Resolve `<rev>` to its full OID hex via `git rev-parse` inside `repo`.
async fn git_rev_parse(repo: &test_harness::Repo, rev: &str) -> Result<String> {
    let out = repo
        .git(["rev-parse", rev])
        .output()
        .await
        .with_context(|| format!("failed to spawn git rev-parse {rev}"))?;
    if !out.status.success() {
        bail!(
            "git rev-parse {rev} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(String::from_utf8(out.stdout)
        .context("git rev-parse returned non-utf8")?
        .trim()
        .to_string())
}

/// Read the OID that `refs/nostr/<event_id_hex>` resolves to inside the bare
/// repository at `bare_repo`. Returns an error if the ref is absent.
async fn read_nostr_ref(bare_repo: &Path, event_id_hex: &str) -> Result<String> {
    let refname = format!("refs/nostr/{event_id_hex}");
    let out = tokio::process::Command::new("git")
        .arg("for-each-ref")
        .arg(&refname)
        .arg("--format=%(objectname)")
        .current_dir(bare_repo)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn `git for-each-ref {refname}` in {}",
                bare_repo.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "`git for-each-ref {refname}` exited non-zero in {}: {}",
            bare_repo.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let oid = String::from_utf8(out.stdout)
        .context("git for-each-ref output is not valid UTF-8")?
        .trim()
        .to_string();
    if oid.is_empty() {
        bail!(
            "ref {refname} not found in bare repo at {} — the update push did not land",
            bare_repo.display(),
        );
    }
    Ok(oid)
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

/// The value of the `branch-name` tag on an event, if present.
fn event_branch_name_tag(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some("branch-name") {
            s.get(1).cloned()
        } else {
            None
        }
    })
}
