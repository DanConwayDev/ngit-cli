//! Companion to `tests/send_pr.rs`: the same `ngit send --force-pr` flow
//! but with the **first** grasp listed in the kind-30617 announcement taken
//! offline immediately before `ngit send` executes.
//!
//! ## Why this test exists
//!
//! `push_refs_and_generate_pr_or_pr_update_event` (push.rs:735-794) builds
//! the unsigned PR event on the first **successful** push, not the first
//! attempted push. When the first listed server goes offline between the clone
//! and the send:
//!
//! - The push loop entry for that server must fail gracefully (no panic / bail)
//!   and advance to the next server.
//! - The unsigned PR event must be built against the **second** server's clone
//!   URL, not the dead first server's URL.
//! - The `clone` tag on the published PR event must therefore reference the
//!   second (live) server, not the first (dead) one.
//!
//! ## Arrangement
//!
//! Same as `send_pr.rs` except:
//!
//! 1. Harness: one vanilla relay (`"default"`), two grasp servers (`"repo"` and
//!    `"repo_secondary"`).
//! 2. Maintainer publishes the repo with **both** grasps in the announcement
//!    (via `PublishRepoOpts::additional_grasp_roles`) — both servers are alive
//!    at this point.
//! 3. A fresh contributor clones — both servers still alive.
//! 4. Contributor commits `t3.md`.
//! 5. **Maintainer advances `main`** (same as `send_pr.rs`) — both servers
//!    still alive.
//! 6. Contributor commits `t4.md`.
//! 7. **Primary grasp (`"repo"`) is taken offline** via `Harness::take_grasp`
//!    + `drop` — `127.0.0.1:<primary_port>` no longer accepts connections.
//! 8. Contributor runs `ngit send HEAD~2 --force-pr`. The contributor's env
//!    (`NGIT_GRASP_DEFAULT_SET`) was snapshot-ted before the kill, so it still
//!    lists the dead server first — exactly the path through the push loop that
//!    must be exercised.
//! 9. [`capture_snapshot`] reads events and git refs; harness drops. Each
//!    `#[rstest]` case asserts on a different slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event is published on the secondary grasp.
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's feature-branch tip OID.
//! 4. The `merge-base` tag equals the fork point (not the current main tip).
//! 5. The `branch-name` tag equals `"feature"`.
//! 6. The PR event has exactly one `clone` tag URL, pointing at the
//!    **secondary** (surviving) grasp server's clone URL — because the first
//!    SUCCESSFUL push determined which server's URL ends up in the event.
//! 7. The secondary grasp's bare repo contains `refs/nostr/<event_id>`
//!    resolving to the contributor's tip OID.
//! 8. No KIND_PULL_REQUEST_UPDATE event was emitted on any live surface.

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
/// `send_pr.rs` (`"pr-test-repo"`) and other PR tests to prevent cross-test
/// relay pollution on the shared vanilla relay surface.
const IDENTIFIER: &str = "pr-failover-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot — captured side-effects of one `ngit send --force-pr` invocation
// ---------------------------------------------------------------------------

/// All observable side-effects of the `ngit send --force-pr` arrangement,
/// captured once during [`capture_snapshot`] and shared read-only across
/// the eight `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST event published by the contributor, read from
    /// the secondary grasp (the only live relay surface at send time).
    /// Assertions 2–6 read from here.
    pr_event: Event,

    /// Number of KIND_PULL_REQUEST events authored by the contributor on the
    /// secondary grasp. Must equal 1 (assertion 1).
    pr_count_secondary: usize,

    /// Total count of KIND_PULL_REQUEST_UPDATE events authored by the
    /// contributor across all live surfaces (secondary grasp and the vanilla
    /// default relay). Must equal 0 (assertion 8).
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
    /// `PublishedRepo::initial_oid`. The `merge-base` tag on the PR event
    /// must equal this (assertion 4).
    merge_base_oid: String,

    /// OID of `main` after the maintainer's "advance main" push. Verified to
    /// differ from `merge_base_oid` so the merge-base assertion cannot pass
    /// trivially by coincidence.
    main_tip_at_send_time: String,

    /// Contributor's feature-branch tip (the `t4.md` commit). The `c` tag
    /// on the PR event must equal this (assertion 3).
    pr_tip_oid: String,

    /// Feature branch name (always `BRANCH`). The `branch-name` tag on the
    /// PR event must equal this (assertion 5).
    branch_name: String,

    /// Full HTTP clone URL of the **secondary** grasp server as it appears
    /// in the kind-30617 announcement's `clone` tag (the second entry, since
    /// the first is the now-dead primary's URL).
    ///
    /// Because `push_refs_and_generate_pr_or_pr_update_event` generates the
    /// unsigned PR event on the first **successful** push (push.rs:736-753),
    /// and the primary server is offline at send time, the event is generated
    /// with the secondary server's URL. That URL must therefore appear as the
    /// sole `clone` value on the PR event's `clone` tag (assertion 6).
    ///
    /// Extracted from the actual announcement rather than constructed so the
    /// assertion is not tautological against URL-construction logic.
    grasp_secondary_clone_url: String,

    /// OID that `refs/nostr/<pr_event_id>` resolves to inside the secondary
    /// grasp's bare repo. Must equal `pr_tip_oid` (assertion 7). The primary
    /// is dead at send time so there is no matching assertion for it.
    grasp_secondary_pr_ref_oid: String,
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
                    .expect("send_pr_first_server_down fixture: capture_snapshot failed"),
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
    //
    // `mut` because `take_grasp` requires mutable access to the roster.
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_grasp_server("repo_secondary")
    .build()
    .await?;

    // --- 1. Maintainer publishes the repo with both grasps -------------------
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("pr-failover maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            additional_grasp_roles: vec!["repo_secondary".into()],
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 2. Extract the secondary clone URL from the announcement ------------
    //
    // The announcement's `clone` tag carries two entries:
    //   [0] = primary ("repo") grasp clone URL
    //   [1] = secondary ("repo_secondary") grasp clone URL
    //
    // When the primary is offline at send time, `push_refs_and_generate_…`
    // succeeds on the secondary and embeds [1] in the PR event. We read this
    // from the actual announcement so the assertion is not tautological against
    // our own URL-construction code.
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
    let grasp_secondary_clone_url = clone_tag_urls.get(1).cloned().context(
        "announcement clone tag has fewer than two URLs — expected [primary, secondary]",
    )?;

    // --- 3. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "pr-failover contributor".into(),
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
    contributor
        .git_ok(
            ["checkout", "-b", BRANCH],
            &format!("git checkout -b {BRANCH}"),
        )
        .await?;
    std::fs::write(contributor.dir().join("t3.md"), "some content\n")
        .context("failed to write t3.md")?;
    contributor
        .git_ok(["add", "t3.md"], "git add t3.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t3.md", "--no-gpg-sign"],
            "git commit -m add t3.md --no-gpg-sign",
        )
        .await?;

    let merge_base_oid = published.initial_oid.clone();

    // --- 5. Maintainer: advance main -----------------------------------------
    //
    // `Repo::nostr_push` is mandatory (not raw `git push`) — see timing rule
    // in `test_harness/src/clock.rs`.
    std::fs::write(publisher.dir().join("t-on-main.md"), "content\n")
        .context("failed to write t-on-main.md")?;
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
        .context("maintainer nostr_push (advance main) failed")?;
    let main_tip_at_send_time = publisher.rev_parse("HEAD").await?;

    // --- 6. Contributor: second commit (t4.md) --------------------------------
    std::fs::write(contributor.dir().join("t4.md"), "some content\n")
        .context("failed to write t4.md")?;
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

    // Pre-condition: all three oids must be distinct.
    if merge_base_oid == main_tip_at_send_time {
        bail!("arrange bug: merge_base_oid == main_tip_at_send_time ({merge_base_oid})");
    }
    if merge_base_oid == pr_tip_oid {
        bail!("arrange bug: merge_base_oid == pr_tip_oid ({merge_base_oid})");
    }
    if main_tip_at_send_time == pr_tip_oid {
        bail!("arrange bug: main_tip_at_send_time == pr_tip_oid ({main_tip_at_send_time})");
    }

    // --- 7. Take primary grasp offline just before the send ------------------
    //
    // `take_grasp` removes "repo" from the harness roster and returns it;
    // `drop` kills the subprocess. After this point the primary's port no
    // longer accepts connections.
    //
    // Critically, `contributor` captured `harness.env()` at clone time so
    // its `NGIT_GRASP_DEFAULT_SET` still lists the dead server first — which
    // is exactly the code path we need to exercise in the push loop.
    let primary_grasp = harness
        .take_grasp("repo")
        .context("primary grasp was already taken or never registered")?;
    let dead_url = primary_grasp.url().to_string();
    drop(primary_grasp);

    // Sanity: confirm the primary is actually down before invoking ngit send.
    let probe = tokio::net::TcpStream::connect(dead_url.trim_start_matches("http://")).await;
    assert!(
        probe.is_err(),
        "primary grasp should be unreachable after drop, but TCP connect to \
         {dead_url} succeeded — cannot test server-down path",
    );

    // --- 8. Contributor: ngit send --force-pr --------------------------------
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

    // --- 9. Capture events from all live surfaces ----------------------------

    // Secondary grasp: KIND_PULL_REQUEST events by contributor.
    let pr_events_secondary = harness
        .grasp("repo_secondary")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count_secondary = pr_events_secondary.len();
    let pr_event = pr_events_secondary
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on secondary grasp after `ngit send --force-pr`",
        )?;

    // Live surfaces: KIND_PULL_REQUEST_UPDATE events by contributor.
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
    let pr_update_count = pr_updates_secondary.len() + pr_updates_relay.len();

    // --- 10. Read git ref from secondary bare repo ---------------------------
    let pr_event_id_hex = pr_event.id.to_hex();
    let grasp_secondary_pr_ref_oid = harness
        .grasp("repo_secondary")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    Ok(Snapshot {
        pr_event,
        pr_count_secondary,
        pr_update_count,
        maintainer_pubkey,
        identifier: IDENTIFIER.to_string(),
        merge_base_oid,
        main_tip_at_send_time,
        pr_tip_oid,
        branch_name: BRANCH.to_string(),
        grasp_secondary_clone_url,
        grasp_secondary_pr_ref_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST event is published by the
/// contributor on the secondary (surviving) grasp server.
///
/// A count > 1 would indicate a duplicate-publish bug or test isolation
/// failure. A count of 0 reaching this assertion is impossible:
/// `capture_snapshot` would already have bailed on the `find` call if no
/// matching event existed.
#[rstest]
#[tokio::test]
async fn pr_event_exactly_one(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count_secondary, 1,
        "expected exactly one KIND_PULL_REQUEST event on secondary grasp authored by \
         contributor; got {}",
        s.pr_count_secondary,
    );
    Ok(())
}

/// Assertion 2: the PR event's `a` tag is the canonical 30617 coordinate
/// pointing at the maintainer's announcement.
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
/// The arrangement advances `main` on the maintainer side (step 5) **after**
/// the contributor has already branched, so `merge_base_oid` (the branch-off
/// commit) differs from `main_tip_at_send_time` (the current remote `main`).
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

/// Assertion 6: the PR event has exactly one `clone` tag URL, and it equals
/// the **secondary** (surviving) grasp server's clone URL.
///
/// `push_refs_and_generate_pr_or_pr_update_event` (push.rs:736-753) generates
/// the unsigned PR event on the first **successful** push. Because the primary
/// server is offline when `ngit send` runs (step 7 drops it), the push attempt
/// to the primary returns `Err` and the event is not generated on that
/// iteration. On the next iteration the secondary succeeds, and the event is
/// generated with the secondary's clone URL. That URL is therefore what appears
/// in the `clone` tag — the property this test was written to guard.
///
/// Asserting the exact URL catches a regression where the server-failure path
/// accidentally keeps the dead primary's URL in the event rather than using
/// the first succeeding server's URL.
#[rstest]
#[tokio::test]
async fn clone_tag_is_secondary_grasp_url(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
        clone_tag_urls[0], s.grasp_secondary_clone_url,
        "PR event's `clone` URL should be the secondary (surviving) grasp server's URL, \
         not the dead primary's; got {:?}, want {:?}",
        clone_tag_urls[0], s.grasp_secondary_clone_url,
    );
    Ok(())
}

/// Assertion 7: the secondary grasp received the git data push — its bare repo
/// has a `refs/nostr/<pr_event_id>` ref resolving to the contributor's tip OID.
///
/// The primary is dead at send time so there is no corresponding assertion for
/// it. The property guarded here is that the surviving server actually received
/// the data (i.e. the push loop did not silently skip all servers on failure).
#[rstest]
#[tokio::test]
async fn secondary_grasp_has_pr_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_secondary_pr_ref_oid, s.pr_tip_oid,
        "secondary grasp: refs/nostr/<pr_event_id> should resolve to pr_tip_oid; \
         got {:?}, want {:?}",
        s.grasp_secondary_pr_ref_oid, s.pr_tip_oid,
    );
    Ok(())
}

/// Assertion 8: no KIND_PULL_REQUEST_UPDATE event was emitted on any live
/// relay surface (secondary grasp or the default vanilla relay).
///
/// `--force-pr` with no `--in-reply-to` always produces a new PR event, never
/// an update. This guards against the two paths being accidentally conflated
/// during refactoring.
#[rstest]
#[tokio::test]
async fn no_pr_update_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events across all live relay surfaces; \
         got {} — was `--force-pr` accidentally routed through the update path?",
        s.pr_update_count,
    );
    Ok(())
}
