//! Load-bearing default-to-PR test for the 9e06e7b change.
//!
//! ## What 9e06e7b changed
//!
//! `generate_patches_or_pr_event_or_pr_updates`
//! (`src/bin/git_remote_nostr/push.rs`) gained the condition
//!
//! ```text
//! || (root_proposal.is_none() && repo_has_grasp_server)
//! ```
//!
//! so that a brand-new `git push origin pr/<branch>` against a repo whose
//! kind-30617 announcement lists at least one GRASP server automatically
//! produces a **KIND_PULL_REQUEST** event (kind 1618) instead of a
//! patch-kind GitPatch event.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`) + one GRASP server (`"repo"`).
//! 2. Maintainer publishes the repo via [`Harness::publish_repo`].
//! 3. A fresh contributor clones and logs in as a new account.
//! 4. Contributor checks out a `pr/feature` branch and makes two commits.
//! 5. Contributor runs `git push -u origin pr/feature` (via
//!    [`Repo::nostr_push`] for timing safety).
//! 6. [`capture_snapshot`] reads all observable side-effects into a
//!    [`Snapshot`] — events on relay surfaces, git refs local to the
//!    contributor, bare-repo refs on the GRASP server, and a fresh `git
//!    ls-remote origin` from a third clone. The harness then drops; grasps and
//!    relay shut down.
//! 7. Each `#[rstest]` case asserts on a different slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event on the GRASP — the 9e06e7b path
//!    fired.
//! 2. Zero Kind::GitPatch events — removing the GRASP check would still emit a
//!    PR event but might also accidentally emit patches if conditions overlap;
//!    this catches that regression.
//! 3. Zero KIND_PULL_REQUEST_UPDATE events — a brand-new push cannot be an
//!    update.
//! 4. Contributor's `refs/remotes/origin/pr/feature` matches the local tip —
//!    `update_remote_refs_pushed` (push.rs:165-170) ran correctly.
//! 5. Contributor's upstream tracking config is set (`-u` behaviour):
//!    `branch.pr/feature.merge = refs/heads/pr/feature`.
//! 6. GRASP bare repo has `refs/nostr/<pr_event_id>` resolving to the tip — the
//!    actual git data push landed.
//! 7. PR event `branch-name` tag equals `"feature"` (the `pr/` prefix is
//!    stripped by `make_branch_name_tag_from_check_out_branch`).
//! 8. PR event `c` tag equals the pushed tip OID.
//! 9. PR event `a` tag encodes the correct 30617 coordinate for the
//!    maintainer's announcement.
//! 10. A fresh nostr-URL clone lists the branch as
//!     `pr/feature(<8-hex-shorthand>)` in its `git ls-remote origin` output.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for the test repo — distinct from other test repos to avoid
/// cross-test relay pollution on the shared vanilla relay.
const IDENTIFIER: &str = "git-push-pr-new-pr";

/// Feature branch name; pushed as `pr/feature`. The `branch-name` tag on the
/// PR event should carry `"feature"` (with the `pr/` prefix stripped).
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot — captured side-effects of one `git push -u origin pr/feature`
// ---------------------------------------------------------------------------

/// All observable side-effects of the push, captured once by
/// [`capture_snapshot`] and shared read-only across the ten `#[rstest]`
/// cases via [`SNAPSHOT`].
struct Snapshot {
    /// The one KIND_PULL_REQUEST event the contributor produced, read from
    /// the GRASP's relay surface. Tag assertions (cases 7–9) read here.
    pr_event: Event,

    /// Total KIND_PULL_REQUEST events authored by the contributor on the
    /// GRASP. Must equal 1 (case 1).
    pr_count: usize,

    /// Total Kind::GitPatch events authored by the contributor across the
    /// GRASP and the default relay. Must equal 0 (case 2).
    patch_count: usize,

    /// Total KIND_PULL_REQUEST_UPDATE events authored by the contributor
    /// across both surfaces. Must equal 0 (case 3).
    pr_update_count: usize,

    /// Contributor's feature-branch tip OID captured immediately after the
    /// last commit (before the push). This is the canonical "what we
    /// pushed" value used by cases 4, 6, and 8.
    contributor_tip_oid: String,

    /// `refs/remotes/origin/pr/feature` from the contributor's repo
    /// snapshot taken after `nostr_push`. Must equal `contributor_tip_oid`
    /// (case 4).
    contributor_remote_tracking_oid: String,

    /// `branch.pr/feature.merge` from the contributor's local git config
    /// after `git push -u`. Must equal `"refs/heads/pr/feature"` (case 5).
    upstream_merge_cfg: String,

    /// OID that `refs/nostr/<pr_event_id>` resolves to in the GRASP's bare
    /// repository (`<git_data_path>/<maintainer_npub>/<identifier>.git`).
    /// Must equal `contributor_tip_oid` (case 6).
    grasp_pr_ref_oid: String,

    /// Maintainer's public key. The `a`-tag on the PR event encodes this
    /// (case 9).
    maintainer_pubkey: PublicKey,

    /// Identifier the repo was published with. The `a`-tag encodes this
    /// (case 9).
    identifier: String,

    /// Hex of the PR event id — used to reconstruct the 8-char shorthand
    /// that `list.rs` appends to the branch name in `git ls-remote` output
    /// (case 10).
    pr_event_id_hex: String,

    /// Refs advertised by a fresh `git ls-remote origin` run against the
    /// nostr URL from a third clone (not the contributor's). Each entry is
    /// `(ref_name, oid)`. Case 10 asserts that
    /// `refs/heads/pr/feature(<8-hex>)` is present and resolves to the tip.
    nostr_clone_ls_refs: BTreeMap<String, String>,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: initialise [`SNAPSHOT`] exactly once per binary, hand
/// every test case a cheap `Arc` clone. Follows the same
/// `OnceCell`-backed pattern as `tests/send_pr.rs` and
/// `tests/git_push_state/fresh_repo.rs`.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("git_push_pr::new_pr fixture: capture_snapshot failed"),
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
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("git push pr maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 3. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "git push pr contributor".into(),
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

    // --- 4. Contributor: check out pr/feature and make two commits -----------
    //
    // The branch name starts with `pr/` so git-remote-nostr routes it
    // through the proposal code path in `push.rs`. The new 9e06e7b condition
    // (`root_proposal.is_none() && repo_has_grasp_server`) then fires
    // because this is the first push of this branch (no existing proposal)
    // and the announcement lists a GRASP server.
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

    let contributor_tip_oid = contributor
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after second commit")?;

    // --- 5. Contributor: push ---------------------------------------------
    //
    // `Repo::nostr_push` ticks one whole unix second before the push so
    // the emitted state events don't collide with the state event published
    // by `publish_repo`.  See `test_harness::clock` for the writeup.
    // `-u` sets the upstream: `branch.pr/feature.{remote,merge}`.
    contributor
        .nostr_push(["-u", "origin", &format!("pr/{BRANCH}")])
        .await
        .context("nostr_push -u origin pr/feature failed")?;

    // --- 6. Capture contributor local state ----------------------------------
    //
    // Snapshot the contributor's refs AFTER the push so that
    // `refs/remotes/origin/pr/feature` is present (it's written by
    // `update_remote_refs_pushed` at push.rs:165-170, acknowledged by git
    // on seeing `ok refs/heads/pr/feature` from the helper).
    let contributor_snap = contributor
        .snapshot()
        .context("capturing contributor snapshot after push")?;
    let remote_tracking_ref = format!("refs/remotes/origin/pr/{BRANCH}");
    let contributor_remote_tracking_oid = contributor_snap
        .refs
        .get(&remote_tracking_ref)
        .with_context(|| {
            format!(
                "{remote_tracking_ref} missing from contributor refs after push — \
                 update_remote_refs_pushed (push.rs:165-170) did not run"
            )
        })?
        .clone();

    let upstream_merge_cfg = contributor
        .config(&format!("branch.pr/{BRANCH}.merge"))
        .await?
        .with_context(|| {
            format!(
                "branch.pr/{BRANCH}.merge not set after `git push -u` — \
                 the -u flag did not write upstream tracking config"
            )
        })?;

    // --- 7. Capture events from the GRASP and relay --------------------------
    let pr_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count = pr_events.len();
    let pr_event = pr_events
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on GRASP after `git push pr/feature`",
        )?;
    let pr_event_id_hex = pr_event.id.to_hex();

    // GitPatch events: count across both surfaces.
    let patch_count = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(Kind::GitPatch),
        )
        .await?
        .len()
        + harness
            .relay("default")
            .events(
                Filter::new()
                    .author(contributor_pubkey)
                    .kind(Kind::GitPatch),
            )
            .await?
            .len();

    // KIND_PULL_REQUEST_UPDATE events: count across both surfaces.
    let pr_update_count = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?
        .len()
        + harness
            .relay("default")
            .events(
                Filter::new()
                    .author(contributor_pubkey)
                    .kind(KIND_PULL_REQUEST_UPDATE),
            )
            .await?
            .len();

    // --- 8. Read the GRASP bare-repo ref before the harness drops ------------
    let grasp_pr_ref_oid = harness
        .grasp("repo")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    // --- 9. Fresh nostr-URL clone: run git ls-remote -------------------------
    //
    // `clone_published_repo` with `CloneLogin::None` produces a repo not
    // logged in as any nostr identity, which forces `list.rs:236` down the
    // long-form `pr/<branch>(<shorthand>)` ref-naming path — matching the
    // assertion in case 10.
    let new_clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    let ls_out = new_clone
        .git(["ls-remote", "origin"])
        .output()
        .await
        .context("failed to spawn git ls-remote origin")?;
    anyhow::ensure!(
        ls_out.status.success(),
        "git ls-remote origin exited {:?}\nstdout: {}\nstderr: {}",
        ls_out.status,
        String::from_utf8_lossy(&ls_out.stdout),
        String::from_utf8_lossy(&ls_out.stderr),
    );
    let ls_stdout =
        String::from_utf8(ls_out.stdout).context("git ls-remote origin stdout is not UTF-8")?;
    let nostr_clone_ls_refs: BTreeMap<String, String> = ls_stdout
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("ref: "))
        .filter_map(|l| l.split_once('\t'))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect();

    Ok(Snapshot {
        pr_event,
        pr_count,
        patch_count,
        pr_update_count,
        contributor_tip_oid,
        contributor_remote_tracking_oid,
        upstream_merge_cfg,
        grasp_pr_ref_oid,
        maintainer_pubkey,
        identifier: IDENTIFIER.to_string(),
        pr_event_id_hex,
        nostr_clone_ls_refs,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Case 1: Exactly one KIND_PULL_REQUEST event is published on the GRASP by
/// the contributor.
///
/// This is the load-bearing assertion for 9e06e7b: without the
/// `repo_has_grasp_server` condition the push would have produced a
/// GitPatch event instead.
#[rstest]
#[tokio::test]
async fn pr_event_exactly_one(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 1,
        "expected exactly one KIND_PULL_REQUEST on the GRASP authored by contributor; \
         got {} — did the 9e06e7b GRASP-default path fire?",
        s.pr_count,
    );
    Ok(())
}

/// Case 2: Zero Kind::GitPatch events on either surface.
///
/// Removing the `repo_has_grasp_server` guard could still leave a code
/// path that emits a PR event while *also* accidentally emitting a patch
/// event if conditions overlap. Asserting zero patches is the explicit
/// regression catch for that scenario.
#[rstest]
#[tokio::test]
async fn zero_patch_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.patch_count, 0,
        "expected zero Kind::GitPatch events across GRASP and default relay; \
         got {} — was a patch event accidentally emitted alongside the PR?",
        s.patch_count,
    );
    Ok(())
}

/// Case 3: Zero KIND_PULL_REQUEST_UPDATE events on either surface.
///
/// A brand-new `pr/feature` branch has no existing proposal on nostr, so
/// the push must produce a new PR event (kind 1618) not an update (kind
/// 1619). An update event requires `--in-reply-to` or an existing
/// matching proposal.
#[rstest]
#[tokio::test]
async fn zero_pr_update_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events across GRASP and default relay; \
         got {} — was the push incorrectly routed through the update path?",
        s.pr_update_count,
    );
    Ok(())
}

/// Case 4: Contributor's `refs/remotes/origin/pr/feature` matches the
/// pushed tip OID.
///
/// `update_remote_refs_pushed` (push.rs:165-170) is called after the
/// helper prints `ok refs/heads/pr/feature` and must write the
/// remote-tracking ref so the contributor's repo reflects what landed on
/// the server.
#[rstest]
#[tokio::test]
async fn contributor_pr_remote_tracking_matches_local(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.contributor_remote_tracking_oid, s.contributor_tip_oid,
        "contributor refs/remotes/origin/pr/{BRANCH} ({}) does not match local tip ({}); \
         update_remote_refs_pushed (push.rs:165-170) may not have run",
        s.contributor_remote_tracking_oid, s.contributor_tip_oid,
    );
    Ok(())
}

/// Case 5: `git push -u` wrote `branch.pr/feature.merge =
/// refs/heads/pr/feature` into the contributor's local config.
///
/// The `-u` flag is handled by git-core (not the remote helper), which
/// sets upstream tracking when the helper reports `ok`. A missing config
/// entry would mean the round-trip broke.
#[rstest]
#[tokio::test]
async fn upstream_tracking_config_set(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let expected = format!("refs/heads/pr/{BRANCH}");
    assert_eq!(
        s.upstream_merge_cfg, expected,
        "branch.pr/{BRANCH}.merge = {:?}, expected {:?}; \
         the -u flag did not set upstream tracking correctly",
        s.upstream_merge_cfg, expected,
    );
    Ok(())
}

/// Case 6: The GRASP bare repo has `refs/nostr/<pr_event_id>` resolving
/// to the contributor's tip OID.
///
/// This is the proof that the git data was actually pushed to the GRASP
/// server — not just the nostr event. Without this ref the branch cannot
/// be fetched from the GRASP URL.
#[rstest]
#[tokio::test]
async fn grasp_has_refs_nostr_for_pr(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_pr_ref_oid,
        s.contributor_tip_oid,
        "GRASP refs/nostr/{} resolves to {} but expected tip {}; \
         git data may not have been pushed to the GRASP",
        &s.pr_event_id_hex[..16],
        s.grasp_pr_ref_oid,
        s.contributor_tip_oid,
    );
    Ok(())
}

/// Case 7: PR event `branch-name` tag equals `"feature"`.
///
/// `make_branch_name_tag_from_check_out_branch` (git_events.rs:657)
/// strips the `pr/` prefix from the checked-out branch name before
/// writing the tag. An incorrect value (e.g. `"pr/feature"`) would break
/// `ngit pr checkout` and `ngit list` ref-resolution logic.
#[rstest]
#[tokio::test]
async fn pr_event_branch_name_tag_is_feature(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "branch-name").as_deref(),
        Some(BRANCH),
        "PR event branch-name tag should be {:?} (pr/ prefix stripped); got {:?}",
        BRANCH,
        tag_value(&s.pr_event, "branch-name"),
    );
    Ok(())
}

/// Case 8: PR event `c` tag equals the contributor's tip OID.
///
/// The `c` tag is what `get_commit_id_from_patch` (git_events.rs:58-60)
/// reads to locate the tip commit for checkout / apply operations. An
/// incorrect value would silently produce the wrong working tree.
#[rstest]
#[tokio::test]
async fn pr_event_c_tag_is_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "c").as_deref(),
        Some(s.contributor_tip_oid.as_str()),
        "PR event c tag should equal contributor's tip OID; got {:?}, want {:?}",
        tag_value(&s.pr_event, "c"),
        s.contributor_tip_oid,
    );
    Ok(())
}

/// Case 9: PR event has an `a` tag encoding the correct 30617 coordinate
/// for the maintainer's announcement.
///
/// The coordinate format is `"30617:<pubkey-hex>:<identifier>"` per NIP-01.
/// An incorrect pubkey component would mean ngit used the wrong
/// announcement as its source of truth; an incorrect identifier would
/// indicate a round-trip bug in the cache.
#[rstest]
#[tokio::test]
async fn pr_event_a_tag_is_repo_coordinate(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let expected = format!("30617:{}:{}", s.maintainer_pubkey, s.identifier);
    let a_tags: Vec<&Tag> = s
        .pr_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("a"))
        .collect();
    assert!(
        a_tags
            .iter()
            .any(|t| t.as_slice().get(1).map(String::as_str) == Some(expected.as_str())),
        "expected an `a` tag with value {expected:?}; found a tags: {:?}",
        a_tags
            .iter()
            .filter_map(|t| t.as_slice().get(1).cloned())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Case 10: A fresh nostr-URL clone (not logged in as the contributor) sees
/// the branch listed as `pr/feature(<8-hex-shorthand>)` in
/// `git ls-remote origin` output.
///
/// `list.rs:236` produces the long-form `pr/<branch>(<event-id-8>)` ref
/// name for any pull request whose author differs from the current logged-in
/// user (or when no user is logged in, as here). The prefix `pr/` is
/// prepended to the `branch-name` tag value by
/// `get_branch_name_with_pr_prefix_and_shorthand_id` (git_events.rs:834-842).
#[rstest]
#[tokio::test]
async fn new_clone_lists_pr_feature_branch(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let shorthand = &s.pr_event_id_hex[..8];
    let expected_ref = format!("refs/heads/pr/{BRANCH}({shorthand})");
    let got_oid = s.nostr_clone_ls_refs.get(&expected_ref).cloned();
    assert_eq!(
        got_oid.as_deref(),
        Some(s.contributor_tip_oid.as_str()),
        "expected fresh clone ls-remote to contain {expected_ref} → {}; \
         got {:?}\nfull ls-remote map: {:#?}",
        s.contributor_tip_oid,
        got_oid,
        s.nostr_clone_ls_refs,
    );
    Ok(())
}
