//! End-to-end coverage of `ngit send --force-pr --git-server <url>` where the
//! supplied git server is a **plain vanilla** (non-grasp) HTTP git server whose
//! URL is not listed in the repo's kind-30617 announcement.
//!
//! ## What this test covers
//!
//! `select_servers_push_refs_and_generate_pr_or_pr_update_event`
//! (push.rs:422-448) accepts an optional `git_server` argument. When the value
//! ends with `.git` (or starts with `git@`) `user_server_is_direct_url` is set
//! and the URL is prepended to `to_try` verbatim — ahead of all the repo's own
//! announcement-listed servers. That first URL then drives two outcomes:
//!
//! 1. The unsigned PR event is generated with that URL in the `clone` tag
//!    (push.rs:648-655 — generated on the first server in `to_try`, reused for
//!    all subsequent ones).
//! 2. Git data is pushed to the vanilla server via `push_to_remote`
//!    (push.rs:687-695 — the `!is_grasp_server_clone_url` branch of
//!    push.rs:664).
//!
//! The test then verifies that the announcement-listed grasp servers ALSO
//! receive the same PR ref (push.rs:465-472 appends them to `to_try` after the
//! user-supplied server, and the inner loop at push.rs:637 iterates every entry
//! so all servers receive the push).
//!
//! ## Arrangement
//!
//! Steps 1–6 mirror `send_pr.rs` exactly so the two tests can be read side by
//! side; the only departure is the `--git-server` flag in step 7.
//!
//! 1. Harness: one vanilla relay (`"default"`), two grasp servers (`"repo"` and
//!    `"repo_secondary"`), one vanilla git server (`"extra"`).
//! 2. Maintainer publishes the repo with **both** grasps in the announcement.
//!    The vanilla git server is **not** mentioned in the announcement.
//! 3. A fresh contributor clones and checks out the `"feature"` branch.
//! 4. Contributor commits `t3.md` — establishes the fork point.
//! 5. **Maintainer advances `main`** so `merge_base_oid ≠
//!    main_tip_at_send_time`.
//! 6. Contributor commits `t4.md`.
//! 7. Contributor runs `ngit send HEAD~2 --force-pr --title … --description …
//!    --git-server http://127.0.0.1:<port>/test-repo.git`.
//! 8. [`capture_snapshot`] reads all events and git refs; harness drops.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event is published.
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's feature-branch tip OID.
//! 4. The `merge-base` tag equals the fork point.
//! 5. The `branch-name` tag equals `"feature"`.
//! 6. The PR event's `clone` tag has exactly one URL equal to the vanilla
//!    server's URL (not any grasp URL).
//! 7. All three servers — both grasps and the vanilla git server — have a
//!    `refs/nostr/<event_id>` ref resolving to the contributor's tip OID.
//! 8. The vanilla server's URL does NOT appear in the repo announcement's
//!    `clone` tag (the server was bypassed at push time, not registered).
//! 9. No KIND_PULL_REQUEST_UPDATE event was emitted on any surface.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value, tag_values,
};
use tokio::sync::OnceCell;

/// Identifier passed to `ngit init --identifier`. Distinct from `send_pr.rs`
/// (`"pr-test-repo"`) and `send_pr_update.rs` (`"pr-update-test-repo"`) to
/// prevent cross-test relay pollution on the shared vanilla relay surface.
const IDENTIFIER: &str = "pr-vanilla-git-server-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

/// Path component appended to the vanilla server's base URL to form a valid
/// `.git`-terminated clone URL. `VanillaGitServer` routes by path suffix
/// (`ends_with("/info/refs")` etc.) so any path works; we pick a descriptive
/// name rather than a bare `/` so the URL is self-documenting in logs.
const VANILLA_REPO_PATH: &str = "/test-repo.git";

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// All observable side-effects of the arrangement, captured once during
/// [`capture_snapshot`] and shared read-only across the nine `#[rstest]`
/// cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST event published by the contributor, read from
    /// the primary grasp. Assertions 2–6 read from here.
    pr_event: Event,

    /// Number of KIND_PULL_REQUEST events authored by the contributor on the
    /// primary grasp. Must equal 1 (assertion 1).
    pr_count_primary: usize,

    /// Total count of KIND_PULL_REQUEST_UPDATE events across all surfaces.
    /// Must equal 0 (assertion 9).
    pr_update_count: usize,

    /// Maintainer's public key. Used to build the expected `a` tag value
    /// (assertion 2).
    maintainer_pubkey: PublicKey,

    /// `d` tag identifier passed to `ngit init`. Used in the `a` tag check
    /// (assertion 2).
    identifier: String,

    /// OID the contributor branched from (parent of the first PR commit).
    /// Must match the `merge-base` tag (assertion 4) and must differ from
    /// both `main_tip_at_send_time` and `pr_tip_oid` (pre-condition).
    merge_base_oid: String,

    /// OID of `main` after the maintainer's "advance main" push. Used only
    /// in the pre-condition check that ensures `merge_base_oid` is distinct.
    main_tip_at_send_time: String,

    /// Contributor's feature-branch tip (the `t4.md` commit). Must match
    /// the `c` tag (assertion 3) and the `refs/nostr/<event_id>` OID on
    /// all three servers (assertion 7).
    pr_tip_oid: String,

    /// Feature branch name (always `BRANCH`). Must match `branch-name` tag
    /// (assertion 5).
    branch_name: String,

    /// The full git-server URL passed to `--git-server`:
    /// `http://127.0.0.1:<port>/test-repo.git`. This is the URL that must
    /// appear (alone) in the PR event's `clone` tag (assertion 6).
    git_server_url: String,

    /// All `clone` tag values from the kind-30617 repo announcement. None
    /// of them must contain the vanilla server's base URL (assertion 8).
    announcement_clone_urls: Vec<String>,

    /// OID at `refs/nostr/<pr_event_id>` in the primary grasp's bare repo.
    /// Must equal `pr_tip_oid` (assertion 7).
    grasp_primary_pr_ref_oid: String,

    /// OID at `refs/nostr/<pr_event_id>` in the secondary grasp's bare repo.
    /// Must equal `pr_tip_oid` (assertion 7).
    grasp_secondary_pr_ref_oid: String,

    /// OID at `refs/nostr/<pr_event_id>` in the vanilla git server's bare
    /// repo. Must equal `pr_tip_oid` (assertion 7).
    vanilla_pr_ref_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary and
/// hand each case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("send_pr_vanilla_git_server fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- Harness: vanilla relay + two grasp servers + one vanilla git
    // --- server --
    //
    // `VanillaGitServer` requires a multi-thread tokio runtime because the
    // accept loop is a spawned task; blocking git pushes from the test
    // thread need a worker thread available while the accept loop services
    // them. Every `#[rstest]` in this file therefore carries
    // `#[tokio::test(flavor = "multi_thread")]`.
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_grasp_server("repo_secondary")
    .with_vanilla_git_server("extra")
    .build()
    .await?;

    // Full clone URL passed to `--git-server`. The path component ends with
    // `.git` so `user_server_is_direct_url` (push.rs:430-432) is `true` and
    // the URL arrives in `to_try` verbatim — no GRASP-06 reformatting.
    let git_server_url = format!(
        "{}{}",
        harness.vanilla_git_server("extra").url(),
        VANILLA_REPO_PATH,
    );

    // --- 1. Maintainer publishes the repo with both grasps -------------------
    //
    // The vanilla git server is deliberately NOT included here. The test
    // verifies it receives push data anyway (via `--git-server`) but is
    // absent from the announcement.
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("pr-vanilla-test maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            additional_grasp_roles: vec!["repo_secondary".into()],
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 2. Extract announcement clone URLs (before the vanilla server push)
    // ---
    //
    // Read these now so the assertion in case 8 is comparing against what
    // the announcement actually contained, not what we expect it to contain.
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
    let announcement_clone_urls = tag_values(announcement, "clone");

    // --- 3. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "pr-vanilla-test contributor".into(),
            },
        )
        .await?;

    // --- 4. Contributor: feature branch + first commit (t3.md) ---------------
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

    // Fork point = parent of the first PR commit = clone-time main tip.
    let merge_base_oid = published.initial_oid.clone();

    // --- 5. Maintainer: advance main -----------------------------------------
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

    // Pre-condition: all three OIDs must be distinct.
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

    // --- 7. Contributor: ngit send --force-pr --git-server <vanilla_url> -----
    //
    // `--git-server http://127.0.0.1:<port>/test-repo.git` ends with `.git`
    // so `user_server_is_direct_url` (push.rs:430) is true: the URL enters
    // `to_try` verbatim, ahead of the repo's grasp servers. The unsigned PR
    // event is generated with this URL as the `clone` hint and the same
    // unsigned event is reused for all subsequent servers in the loop.
    let send_out = contributor
        .ngit([
            "send",
            "HEAD~2",
            "--force-pr",
            "--title",
            "add feature",
            "--description",
            "this adds the feature",
            "--git-server",
            &git_server_url,
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-pr --git-server")?;
    if !send_out.status.success() {
        bail!(
            "ngit send --force-pr --git-server exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            send_out.status,
            String::from_utf8_lossy(&send_out.stdout),
            String::from_utf8_lossy(&send_out.stderr),
        );
    }

    // --- 8. Capture events from all surfaces ---------------------------------

    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after AsContributor login")?;
    let contributor_keys =
        Keys::parse(&contributor_nsec).context("contributor nostr.nsec is not a valid key")?;
    let contributor_pubkey = contributor_keys.public_key();

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
             found on primary grasp after `ngit send --force-pr --git-server`",
        )?;

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

    // --- 9. Read git refs from both grasps and the vanilla server -----------
    //
    // All three bare-repo dirs must be read while the harness is still alive
    // (TempDirs drop on harness drop).
    let pr_event_id_hex = pr_event.id.to_hex();

    let grasp_primary_pr_ref_oid = harness
        .grasp("repo")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    let grasp_secondary_pr_ref_oid = harness
        .grasp("repo_secondary")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    // The vanilla server routes ALL requests to a single bare repo regardless
    // of the URL path — `handle_request` dispatches on `path.ends_with(…)`.
    // So the push to `http://…/test-repo.git` lands in the same bare repo
    // that `repo_path()` points at.
    let vanilla_pr_ref_oid = harness
        .vanilla_git_server("extra")
        .read_nostr_ref(&pr_event_id_hex)
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
        git_server_url,
        announcement_clone_urls,
        grasp_primary_pr_ref_oid,
        grasp_secondary_pr_ref_oid,
        vanilla_pr_ref_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST event is published by the
/// contributor on the primary grasp.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
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

/// Assertion 2: the `a` tag encodes the maintainer's repo coordinate.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
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

/// Assertion 3: the `c` tag equals the contributor's feature-branch tip OID.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
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

/// Assertion 4: the `merge-base` tag equals the fork point, not `main` tip.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
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

/// Assertion 5: the `branch-name` tag equals `"feature"`.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
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

/// Assertion 6: the PR event's `clone` tag has exactly one URL and it is the
/// vanilla git server's URL (not a grasp URL).
///
/// Because `user_server_is_direct_url` is true when the URL ends with `.git`
/// (push.rs:430-432), the supplied URL enters `to_try` first (push.rs:438).
/// `push_refs_and_generate_pr_or_pr_update_event` generates the unsigned PR
/// event on the **first successful push** and reuses it for all subsequent
/// servers (push.rs:638-655), so the vanilla URL is the sole `clone` value.
///
/// A regression where the grasp URL appeared here would mean the server
/// iteration order changed or the event was regenerated on a later server.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn clone_tag_is_vanilla_git_server_url(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
        clone_tag_urls[0], s.git_server_url,
        "PR event's `clone` URL should be the vanilla git server URL (not a grasp URL); \
         got {:?}, want {:?}",
        clone_tag_urls[0], s.git_server_url,
    );
    Ok(())
}

/// Assertion 7: all three servers received the git data push — both grasps
/// and the vanilla git server each have `refs/nostr/<pr_event_id>` resolving
/// to the contributor's tip OID.
///
/// The loop in `push_refs_and_generate_pr_or_pr_update_event` (push.rs:637)
/// iterates every entry in `to_try`, which is `[vanilla, grasp_primary,
/// grasp_secondary]`. Pushing to each server but only crediting the first in
/// the `clone` tag is exactly the multi-server push contract; a premature
/// `break` or off-by-one in the loop would leave one or more servers with a
/// missing ref.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn all_three_servers_have_pr_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
    assert_eq!(
        s.vanilla_pr_ref_oid, s.pr_tip_oid,
        "vanilla git server: refs/nostr/<pr_event_id> should resolve to pr_tip_oid; \
         got {:?}, want {:?}",
        s.vanilla_pr_ref_oid, s.pr_tip_oid,
    );
    Ok(())
}

/// Assertion 8: the vanilla git server's URL does not appear in the repo
/// announcement's `clone` tag.
///
/// The server was supplied at send-time via `--git-server`; it was never
/// passed to `ngit init --grasp-server` and is therefore not part of the
/// kind-30617 announcement. This assertion guards against a regression where
/// `select_servers_push_refs_and_generate_pr_or_pr_update_event` or
/// `apply_grasp_infrastructure` accidentally injected the user-supplied URL
/// back into the announcement.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn vanilla_server_not_in_announcement(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    // Strip the path component to get the bare `http://host:port` base —
    // if any announcement URL even *starts with* the vanilla server's address
    // something has gone wrong.
    let vanilla_base = s
        .git_server_url
        .strip_suffix(VANILLA_REPO_PATH)
        .unwrap_or(&s.git_server_url);
    for url in &s.announcement_clone_urls {
        assert!(
            !url.contains(vanilla_base),
            "vanilla git server base URL ({vanilla_base:?}) should not appear in the \
             repo announcement's clone tag, but found it in {url:?}",
        );
    }
    Ok(())
}

/// Assertion 9: no KIND_PULL_REQUEST_UPDATE event was emitted on any surface.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
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
