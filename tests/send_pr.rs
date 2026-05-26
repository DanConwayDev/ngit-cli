//! End-to-end coverage of `ngit send --force-pr` against a repo whose
//! kind-30617 announcement lists **two** grasp servers.
//!
//! ## Why this test exists
//!
//! `push_refs_and_generate_pr_or_pr_update_event` (push.rs:735-794) and
//! `select_servers_push_refs_and_generate_pr_or_pr_update_event` (push.rs:412)
//! contain two distinct paths that are both present in the current codebase and
//! must be preserved through an upcoming refactor:
//!
//! 1. **GRASP happy path** (this test): repo announcement lists grasp servers;
//!    contributor pushes git data to each of them and publishes a single
//!    KIND_PULL_REQUEST event whose `clone` tag carries the *first* grasp's
//!    clone URL (push.rs:735-749 reuses the unsigned event from the first
//!    server for all subsequent servers).
//!
//! 2. **Personal-fork fallback** (push.rs:504-702): no writable grasp in the
//!    announcement; ngit creates a personal-fork repo announcement and pushes
//!    to the contributor's own grasp servers instead. **Not tested here.**
//!
//! The personal-fork path is scheduled for replacement with GRASP-06
//! `/prs/<contributor-npub>/<identifier>.git` semantics; the GRASP happy path
//! (this file) must survive that refactor unchanged.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`), two grasp servers (`"repo"` and
//!    `"repo_secondary"`).
//! 2. Maintainer publishes the repo with **both** grasps in the announcement
//!    (via `PublishRepoOpts::additional_grasp_roles`).
//! 3. A fresh contributor clones and checks out a `"feature"` branch.
//! 4. Contributor commits `t3.md` — establishes the first PR commit.
//! 5. **Maintainer advances `main`** by committing `t-on-main.md` and running
//!    `Repo::nostr_push` so the tip diverges from the contributor's fork point.
//!    This makes `merge_base_oid ≠ main_tip_at_send_time`, without which the
//!    merge-base assertion would pass trivially.
//! 6. Contributor commits `t4.md` (feature branch now 2 ahead of their local
//!    `main`, which is 1 behind the maintainer-advanced remote).
//! 7. Contributor runs `ngit send HEAD~2 --force-pr --title … --description …`.
//! 8. [`capture_snapshot`] reads all relevant events and git refs into a
//!    [`Snapshot`] struct; the harness then drops (grasps and relay shut down).
//!    Each `#[rstest]` case asserts on a different slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event is published.
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's feature-branch tip OID.
//! 4. The `merge-base` tag equals the fork point (not the current `main` tip).
//! 5. The `branch-name` tag equals `"feature"`.
//! 6. The PR event has exactly one `clone` tag URL, pointing at the *first*
//!    (priority) grasp server's clone URL.
//! 7. Both grasp servers' git data directories contain a
//!    `refs/nostr/<event_id>` ref resolving to the contributor's tip OID.
//! 8. No KIND_PULL_REQUEST_UPDATE event was emitted on any surface.
//!
//! ## Extension notes
//!
//! PR Update and PR-update-with-rebase are out of scope here; they will
//! land in follow-up PRs. The assertion helpers below (`tag_value`,
//! `tag_values`, `event_branch_name_tag`) are intentionally kept small and
//! free of PR-update-specific logic so they can be shared by copy when
//! the follow-up is authored.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value, tag_values,
};
use tokio::sync::OnceCell;

/// Identifier passed to `ngit init --identifier`. Deliberately distinct from
/// the harness default (`"ngit-test-repo"`) to prevent cross-test relay
/// pollution on the shared vanilla relay surface.
const IDENTIFIER: &str = "pr-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot — captured side-effects of one `ngit send --force-pr` invocation
// ---------------------------------------------------------------------------

/// All observable side-effects of the `ngit send --force-pr` arrangement,
/// captured once during [`capture_snapshot`] and shared read-only across
/// the eight `#[rstest]` cases via [`SNAPSHOT`].
///
/// Fields are documented with the assertion(s) they serve so it's obvious
/// which cases would be affected if a field ever needs to change.
struct Snapshot {
    /// The KIND_PULL_REQUEST event published by the contributor, read from
    /// the primary grasp's relay surface. Assertions 2–6 read from here.
    pr_event: Event,

    /// Number of KIND_PULL_REQUEST events authored by the contributor on the
    /// primary grasp. Must equal 1 (assertion 1). Captured separately so the
    /// single-event assertion can run independently of the tag assertions.
    pr_count_primary: usize,

    /// Total count of KIND_PULL_REQUEST_UPDATE events authored by the
    /// contributor across all three surfaces (primary grasp, secondary grasp,
    /// and the vanilla default relay). Must equal 0 (assertion 8).
    pr_update_count: usize,

    /// Maintainer's public key. The `a` tag on the PR event must encode this
    /// as its `<pubkey-hex>` component (assertion 2).
    maintainer_pubkey: PublicKey,

    /// `d` tag identifier that was passed to `ngit init --identifier`.
    /// The `a` tag on the PR event must encode this as its `<identifier>`
    /// component (assertion 2).
    identifier: String,

    /// OID of the commit the contributor branched off from — i.e. the parent
    /// of the first PR commit (`t3.md`). Derived from
    /// `PublishedRepo::initial_oid` (which is `main` when the contributor
    /// cloned). The `merge-base` tag on the PR event must equal this
    /// (assertion 4). Also used in the pre-condition check: must differ from
    /// both `main_tip_at_send_time` and `pr_tip_oid`.
    merge_base_oid: String,

    /// OID of `main` after the maintainer's "advance main" push. Verified
    /// during capture to differ from `merge_base_oid` so the merge-base
    /// assertion (assertion 4) cannot pass trivially by coincidence.
    main_tip_at_send_time: String,

    /// Contributor's feature-branch tip (the `t4.md` commit). The `c` tag
    /// on the PR event must equal this (assertion 3).
    pr_tip_oid: String,

    /// Feature branch name (always `BRANCH`). The `branch-name` tag on the
    /// PR event must equal this (assertion 5).
    branch_name: String,

    /// Full HTTP clone URL of the primary (first) grasp server as it appears
    /// in the kind-30617 announcement's `clone` tag. Because
    /// `push_refs_and_generate_pr_or_pr_update_event` generates the
    /// unsigned PR event on the **first** successful push and reuses it for
    /// all subsequent servers (push.rs:736-753), this URL ends up as the sole
    /// `clone` value on the PR event's `clone` tag (assertion 6).
    ///
    /// Shape: `http://127.0.0.1:<port>/<maintainer_npub>/<identifier>.git`.
    /// Extracted from the actual announcement rather than constructed so the
    /// assertion is not tautological against the URL-construction logic.
    grasp_primary_clone_url: String,

    /// OID that `refs/nostr/<pr_event_id>` resolves to inside the primary
    /// grasp's bare repo
    /// (`<git_data_path>/<maintainer_npub>/<identifier>.git`). Must equal
    /// `pr_tip_oid` (assertion 7, primary half). Read before the
    /// harness drops so the on-disk bare repo dirs are still alive.
    grasp_primary_pr_ref_oid: String,

    /// OID that `refs/nostr/<pr_event_id>` resolves to inside the secondary
    /// grasp's bare repo. Must equal `pr_tip_oid` (assertion 7, secondary
    /// half). The redundant-push property this assertion guards is the
    /// load-bearing one for users running multiple grasps for redundancy.
    grasp_secondary_pr_ref_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary via
/// [`SNAPSHOT`] and hand each test case a cheap `Arc` clone.
///
/// Follows the same `OnceCell`-backed pattern as `tests/init_state_fresh.rs`
/// and `tests/init_state_co_maintainer.rs`: 8 expensive assertions on 1
/// expensive arrange, so we share the arrange step.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("send_pr fixture: capture_snapshot failed"),
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

    // --- 1. Maintainer publishes the repo with both grasps -------------------
    //
    // `additional_grasp_roles: ["repo_secondary"]` appends a second
    // `--grasp-server <url>` to `ngit init` so the kind-30617 `clone` tag
    // carries two URLs: [repo, repo_secondary]. The order matches the CLI args
    // order (see `PublishRepoOpts::additional_grasp_roles` doc for the ordering
    // rule).
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("pr-test maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            additional_grasp_roles: vec!["repo_secondary".into()],
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 2. Extract the primary clone URL from the announcement ---------------
    //
    // We read this from the actual announcement (rather than constructing it
    // from `grasp.url() + npub + identifier`) so the assertion in case 6 is
    // not tautological against our own URL-construction code. The announcement
    // is on the default relay because `publish_repo` graduates it there via
    // the nostr_push that follows `ngit init`.
    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .iter()
        .find(|e| tag_value(e, "d").as_deref() == Some(IDENTIFIER))
        .context("no kind-30617 with expected identifier on default relay after publish_repo")?;
    let clone_tag_urls = tag_values(announcement, "clone");
    let grasp_primary_clone_url = clone_tag_urls
        .into_iter()
        .next()
        .context("announcement's clone tag is empty — no primary grasp URL was embedded")?;

    // --- 3. Clone as a fresh contributor --------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "pr-test contributor".into(),
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

    // --- 4. Contributor: feature branch + first commit (t3.md) ---------------
    //
    // After this step the contributor's `main` is at `published.initial_oid`.
    // The parent of this first PR commit is therefore `published.initial_oid`,
    // which becomes the `merge_base_oid`. We do NOT push back to main or fetch
    // here — the contributor's local view of main stays at the clone-time tip.
    contributor
        .git_ok(
            ["checkout", "-b", BRANCH],
            &format!("git checkout -b {BRANCH}"),
        )
        .await?;
    std::fs::write(contributor.dir().join("t3.md"), "some content\n")
        .context("failed to write t3.md in contributor clone")?;
    contributor
        .git_ok(["add", "t3.md"], "git add t3.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t3.md", "--no-gpg-sign"],
            "git commit -m add t3.md --no-gpg-sign",
        )
        .await?;

    // merge_base = parent of the first PR commit = clone-time main tip.
    // `publish_repo`'s `initial_oid` is the seed commit's oid; the clone
    // lands at exactly that oid, so the contributor branched off there.
    let merge_base_oid = published.initial_oid.clone();

    // --- 5. Maintainer: advance main ------------------------------------------
    //
    // A new commit on the maintainer's `main`, pushed via `Repo::nostr_push`
    // so both grasps update their `refs/heads/main` AND a fresh kind-30618
    // state event is published. After this step, the maintainer's `main` tip
    // is one commit ahead of `merge_base_oid` — the gap that makes the
    // merge-base assertion non-trivial.
    //
    // `Repo::nostr_push` is mandatory here (not raw `git push`); it sleeps
    // one whole unix second before pushing so the new kind-30618 state event
    // can't collide with the one published by `publish_repo`. See
    // `test_harness/src/clock.rs` for the writeup.
    std::fs::write(publisher.dir().join("t-on-main.md"), "content\n")
        .context("failed to write t-on-main.md on publisher side")?;
    publisher
        .git_ok(["add", "t-on-main.md"], "git add t-on-main.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main", "--no-gpg-sign"],
            "git commit -m advance main --no-gpg-sign",
        )
        .await?;
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("maintainer nostr_push to advance main failed")?;
    let main_tip_at_send_time = publisher.rev_parse("HEAD").await?;

    // --- 6. Contributor: second commit (t4.md) --------------------------------
    //
    // The contributor does NOT fetch — their local `main` is still at
    // `merge_base_oid`. After this commit the feature branch is 2 ahead of
    // the contributor's local main, which is 1 behind the maintainer-advanced
    // remote main. `ngit send HEAD~2` will include both t3.md and t4.md.
    std::fs::write(contributor.dir().join("t4.md"), "some content\n")
        .context("failed to write t4.md in contributor clone")?;
    contributor
        .git_ok(["add", "t4.md"], "git add t4.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t4.md", "--no-gpg-sign"],
            "git commit -m add t4.md --no-gpg-sign",
        )
        .await?;
    let pr_tip_oid = contributor.rev_parse("HEAD").await?;

    // Pre-condition: all three oids must be distinct. If any two are equal the
    // arrangement has a bug and the merge-base assertion would pass trivially.
    if merge_base_oid == main_tip_at_send_time {
        bail!(
            "arrange bug: merge_base_oid == main_tip_at_send_time ({merge_base_oid}); \
             the maintainer's 'advance main' commit must produce a distinct oid"
        );
    }
    if merge_base_oid == pr_tip_oid {
        bail!(
            "arrange bug: merge_base_oid == pr_tip_oid ({merge_base_oid}); \
             the contributor's feature-branch tip must differ from the fork point"
        );
    }
    if main_tip_at_send_time == pr_tip_oid {
        bail!(
            "arrange bug: main_tip_at_send_time == pr_tip_oid ({main_tip_at_send_time}); \
             the two trees must diverge after step 5"
        );
    }

    // --- 7. Contributor: ngit send --force-pr --------------------------------
    //
    // `HEAD~2` includes the two commits (t3.md and t4.md) above the fork
    // point. `--force-pr` bypasses the commit-size heuristic in
    // `src/bin/ngit/sub_commands/send.rs:236-243` that would otherwise
    // decide the kind from payload size. `--title` / `--description` satisfy
    // `validate_send_args`'s non-interactive requirement.
    let send_out = contributor
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
        .context("failed to spawn ngit send --force-pr")?;
    if !send_out.status.success() {
        bail!(
            "ngit send --force-pr exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            send_out.status,
            String::from_utf8_lossy(&send_out.stdout),
            String::from_utf8_lossy(&send_out.stderr),
        );
    }

    // --- 8. Capture events from all surfaces ---------------------------------

    // Primary grasp: KIND_PULL_REQUEST events by contributor.
    let pr_events_primary = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count_primary = pr_events_primary.len();
    let pr_event = pr_events_primary
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on primary grasp after `ngit send --force-pr`",
        )?;

    // All three surfaces: KIND_PULL_REQUEST_UPDATE events by contributor.
    let pr_updates_primary = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_updates_secondary = harness
        .grasp("repo_secondary")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_updates_relay = harness
        .relay("default")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_update_count =
        pr_updates_primary.len() + pr_updates_secondary.len() + pr_updates_relay.len();

    // --- 9. Read git refs from both grasps before harness drops ---------------
    //
    // `GraspServer::git_data_path()` is backed by a `TempDir` inside the
    // `GraspServer` struct; once the harness drops, the path is cleaned up.
    // We read both grasps' bare repos here, while they are still alive.
    //
    // Layout: `<git_data_path>/<maintainer_npub>/<identifier>.git` — the same
    // path proves in `tests/clone_grasp.rs` and `tests/init_grasp.rs`.
    let pr_event_id_hex = pr_event.id.to_hex();
    let grasp_primary_pr_ref_oid = harness
        .grasp("repo")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    let grasp_secondary_pr_ref_oid = harness
        .grasp("repo_secondary")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    Ok(Snapshot {
        pr_event,
        pr_count_primary,
        pr_update_count,
        maintainer_pubkey,
        identifier: IDENTIFIER.to_string(),
        merge_base_oid,
        main_tip_at_send_time,
        pr_tip_oid,
        branch_name: BRANCH.to_string(),
        grasp_primary_clone_url,
        grasp_primary_pr_ref_oid,
        grasp_secondary_pr_ref_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST event is published, authored
/// by the contributor, on the primary grasp.
///
/// A count > 1 would indicate either a duplicate-publish bug in ngit or a
/// test-isolation failure (stale events from a prior run on a shared relay).
/// A count of 0 reaching this assertion would mean `capture_snapshot` bailed
/// before returning; this case only fires when there is exactly one event with
/// the wrong branch-name but nonzero total count.
#[rstest]
#[tokio::test]
async fn pr_event_exactly_one(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count_primary, 1,
        "expected exactly one KIND_PULL_REQUEST event on primary grasp authored by \
         contributor; got {}",
        s.pr_count_primary,
    );
    Ok(())
}

/// Assertion 2: the PR event's `a` tag is the canonical 30617 coordinate
/// pointing at the maintainer's announcement.
///
/// The coordinate format is `"30617:<pubkey-hex>:<identifier>"` per NIP-01.
/// Generated by `generate_unsigned_pr_or_update_event` (git_events.rs:586-603)
/// from `repo_ref.maintainers` and `repo_ref.identifier`. An incorrect pubkey
/// component would mean ngit used the wrong announcement as its source of
/// truth; an incorrect identifier would mean the identifier round-tripped
/// incorrectly through the cache.
#[rstest]
#[tokio::test]
async fn a_tag_is_repo_coordinate(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
        "expected an `a` tag with value {expected:?}; got a tags: {:?}",
        a_tags
            .iter()
            .filter_map(|t| t.as_slice().get(1).cloned())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Assertion 3: the PR event's `c` tag equals the contributor's feature-branch
/// tip OID.
///
/// The `c` tag is what `get_commit_id_from_patch` (git_events.rs:58-60) reads
/// to find the tip commit for checkout / apply operations. An incorrect value
/// would silently produce the wrong working tree after `ngit pr checkout`.
#[rstest]
#[tokio::test]
async fn c_tag_is_pr_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "c").as_deref(),
        Some(s.pr_tip_oid.as_str()),
        "PR event `c` tag should equal contributor's feature-branch tip OID; \
         got {:?}, want {:?}",
        tag_value(&s.pr_event, "c"),
        s.pr_tip_oid,
    );
    Ok(())
}

/// Assertion 4: the PR event's `merge-base` tag equals the OID the contributor
/// branched from, NOT the current `main` tip.
///
/// `select_servers_push_refs_and_generate_pr_or_pr_update_event` passes
/// `git_repo.get_commit_parent(first_commit).ok().as_ref()` as the
/// `merge_base` argument (send.rs:367). This is the parent of the **first PR
/// commit**, which is the commit at which the contributor branched off of main.
///
/// The arrangement specifically advances `main` on the maintainer side (step 5)
/// **after** the contributor has already branched. This creates a scenario
/// where `merge_base_oid` (the branch-off commit) differs from
/// `main_tip_at_send_time` (the current remote `main`). A bug that feeds
/// "current main tip" or "HEAD" instead of `get_commit_parent(first_commit)`
/// would produce the wrong merge base — exactly the regression this assertion
/// is designed to catch.
///
/// The pre-condition check in `capture_snapshot` verifies that
/// `merge_base_oid ≠ main_tip_at_send_time` so this assertion can only pass
/// trivially if both the test arrangement and the production code are broken
/// in exactly the same way (extremely unlikely).
#[rstest]
#[tokio::test]
async fn merge_base_tag_is_fork_point(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "merge-base").as_deref(),
        Some(s.merge_base_oid.as_str()),
        "PR event `merge-base` tag should equal the contributor's fork-point OID \
         (parent of the first PR commit, not the current main tip);\n\
         got {:?}\nwant: {:?}\nmain_tip_at_send_time was: {}",
        tag_value(&s.pr_event, "merge-base"),
        s.merge_base_oid,
        s.main_tip_at_send_time,
    );
    Ok(())
}

/// Assertion 5: the PR event's `branch-name` tag equals the contributor's
/// feature branch name (`"feature"`).
///
/// Generated by `make_branch_name_tag_from_check_out_branch`
/// (git_events.rs:647) from `git_repo.get_checked_out_branch_name()` at send
/// time. An incorrect value would break `ngit pr checkout` and `ngit list`
/// ref-resolution logic.
#[rstest]
#[tokio::test]
async fn branch_name_tag_matches(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "branch-name").as_deref(),
        Some(s.branch_name.as_str()),
        "PR event `branch-name` tag should equal the contributor's feature branch name; \
         got {:?}, want {:?}",
        tag_value(&s.pr_event, "branch-name"),
        s.branch_name,
    );
    Ok(())
}

/// Assertion 6: the PR event has exactly one `clone` tag URL and it equals
/// the **first** (priority) grasp server's clone URL.
///
/// `push_refs_and_generate_pr_or_pr_update_event` (push.rs:736-753) generates
/// the unsigned PR event on the first server and reuses it for all subsequent
/// ones — so only the first server's URL ends up in the `clone` tag even
/// though git data is pushed to every server. The invariant being protected:
/// the `clone` tag in a PR event is the submitter's canonical git-data URL,
/// used by `pr_event_clone_tag_urls` (git_events.rs:41) to locate commits
/// during `ngit pr checkout` / `ngit pr apply`.
///
/// Asserting the exact URL (not just its shape) catches a regression where
/// the server-iteration order changes and a different grasp's URL slips in.
#[rstest]
#[tokio::test]
async fn clone_tag_is_primary_grasp_url(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let clone_tag_urls = tag_values(&s.pr_event, "clone");
    assert_eq!(
        clone_tag_urls.len(),
        1,
        "expected exactly one URL in PR event's `clone` tag; got {}: {:?}",
        clone_tag_urls.len(),
        clone_tag_urls,
    );
    assert_eq!(
        clone_tag_urls[0], s.grasp_primary_clone_url,
        "PR event's `clone` URL should be the first grasp server's URL (not the \
         secondary); got {:?}, want {:?}",
        clone_tag_urls[0], s.grasp_primary_clone_url,
    );
    Ok(())
}

/// Assertion 7: both grasp servers received the git data push — each bare repo
/// has a `refs/nostr/<pr_event_id>` ref resolving to the contributor's tip OID.
///
/// This is the redundancy property that matters most for production
/// deployments: a user running two announcement-listed grasps expects the PR to
/// be fetchable from either one independently. A regression that stops the push
/// loop in `push_refs_and_generate_pr_or_pr_update_event` from iterating to the
/// second server (e.g. a premature `break` or a changed iteration order) would
/// silently half-break multi-grasp setups. Since ngit tests run single-grasp
/// almost everywhere, coverage here fills a non-trivial blind spot.
#[rstest]
#[tokio::test]
async fn both_grasps_have_pr_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_primary_pr_ref_oid, s.pr_tip_oid,
        "primary grasp: refs/nostr/<pr_event_id> should resolve to pr_tip_oid; \
         got {:?}, want {:?}",
        s.grasp_primary_pr_ref_oid, s.pr_tip_oid,
    );
    assert_eq!(
        s.grasp_secondary_pr_ref_oid, s.pr_tip_oid,
        "secondary grasp: refs/nostr/<pr_event_id> should resolve to pr_tip_oid; \
         got {:?}, want {:?}",
        s.grasp_secondary_pr_ref_oid, s.pr_tip_oid,
    );
    Ok(())
}

/// Assertion 8: no KIND_PULL_REQUEST_UPDATE event was emitted on any relay
/// surface (primary grasp, secondary grasp, or the default vanilla relay).
///
/// `--force-pr` with no `--in-reply-to` always produces a new PR event, never
/// an update. An update event (kind 1619) requires `--in-reply-to
/// <existing_pr>` which routes through a different code path in send.rs.
/// Asserting count == 0 here ensures the two paths are not accidentally
/// conflated during refactoring.
#[rstest]
#[tokio::test]
async fn no_pr_update_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events across all relay surfaces; \
         got {} — was `--force-pr` accidentally routed through the update path?",
        s.pr_update_count,
    );
    Ok(())
}
