//! All repo servers offline at send time: contributor's user grasp list used.
//!
//! ## Why this test exists
//!
//! When every server listed in the kind-30617 repo announcement is offline,
//! `select_servers_push_refs_and_generate_pr_or_pr_update_event`
//! (push.rs:411+) exhausts all repo grasps and then consults the contributor's
//! `KIND_USER_GRASP_LIST` (kind 10317) for untried personal grasp servers.
//! Rather than creating a personal-fork announcement (kind 30617), it pushes
//! git data to the GRASP-06 `/prs/<contributor-npub>/<identifier>.git`
//! endpoint on those servers and generates a PR event whose `clone` tag
//! references that endpoint.
//!
//! This test verifies that end-to-end path:
//!
//! - The PR event's `clone` tag references the GRASP-06 `/prs/` URL on the user
//!   grasp server, **not** any dead repo server.
//! - The git data (`refs/nostr/<event_id>`) actually landed in the
//!   `prs/<contributor-hex>/<identifier>.git` layout on the user grasp server.
//! - No kind-30617 announcement was emitted by the contributor — the personal-
//!   fork fallback code path must be entirely dead.
//!
//! ## Note on repo announcement
//!
//! After the GRASP-06 fallback lands, the contributor must publish NO
//! kind-30617 at all; assertion 9 enforces this. Any regression that
//! accidentally re-enables the personal-fork code path will cause assertion 9
//! to fail with an explicit message naming the kind.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`), one repo grasp (`"repo"`), one
//!    GRASP-06-enabled user-grasp server (`"user_grasp"`).
//! 2. Maintainer publishes the repo using only the `"repo"` grasp.
//! 3. A fresh contributor clones and logs in.
//! 4. Contributor's user grasp list is published to the default relay, pointing
//!    at the `"user_grasp"` server.
//! 5. Contributor commits `t3.md` on a feature branch.
//! 6. Maintainer advances `main` (same as `send_pr.rs`).
//! 7. Contributor commits `t4.md`.
//! 8. **Primary (only) grasp (`"repo"`) is taken offline** — all repo servers
//!    are now dead.
//! 9. Contributor runs `ngit send HEAD~2 --force-pr`. Because the user grasp
//!    list lists `"user_grasp"` (GRASP-06 enabled), ngit pushes to the
//!    `/prs/<contributor-npub>/<identifier>.git` endpoint and uses that URL in
//!    the PR event's `clone` tag. No kind-30617 is created.
//! 10. [`capture_snapshot`] reads events and git refs; harness drops.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event by the contributor on the default
//!    relay (where send.rs publishes via the user's write relays).
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's feature-branch tip OID.
//! 4. The `merge-base` tag equals the fork point (not the current main tip).
//! 5. The `branch-name` tag equals `"feature"`.
//! 6. The PR event has exactly one `clone` tag URL, pointing at the GRASP-06
//!    `/prs/<contributor-npub>/<identifier>.git` endpoint on the user grasp
//!    server.
//! 7. The user grasp's bare repo at `prs/<contributor-hex>/<identifier>.git`
//!    contains `refs/nostr/<event_id>` resolving to the contributor's tip OID.
//! 8. No KIND_PULL_REQUEST_UPDATE event was emitted on any live surface.
//! 9. No kind-30617 announcement was emitted by the contributor — the
//!    personal-fork fallback is entirely absent.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value, tag_values,
};
use tokio::sync::OnceCell;

/// Identifier passed to `ngit init --identifier`. Distinct from every other
/// PR test to prevent cross-test relay pollution on the shared vanilla relay.
const IDENTIFIER: &str = "pr-all-down-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// All observable side-effects captured once and shared across the nine
/// `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST event published by the contributor, read from
    /// the default relay (send.rs publishes to `user_ref.relays.write()`
    /// which includes the default relay).
    pr_event: Event,

    /// Number of KIND_PULL_REQUEST events authored by the contributor on
    /// the default relay. Must equal 1 (assertion 1).
    pr_count_default: usize,

    /// Total count of KIND_PULL_REQUEST_UPDATE events authored by the
    /// contributor across all live surfaces (default relay and user_grasp
    /// relay). Must equal 0 (assertion 8).
    pr_update_count: usize,

    /// Maintainer's public key. Used to build the expected `a` tag value
    /// (assertion 2).
    maintainer_pubkey: PublicKey,

    /// `d` tag identifier that was passed to `ngit init --identifier`.
    identifier: String,

    /// OID of the commit the contributor branched from — the parent of the
    /// first PR commit (`t3.md`). Derived from `PublishedRepo::initial_oid`.
    /// The `merge-base` tag on the PR event must equal this (assertion 4).
    merge_base_oid: String,

    /// OID of `main` after the maintainer's "advance main" push. Verified
    /// to differ from `merge_base_oid` so the merge-base assertion cannot
    /// pass trivially.
    main_tip_at_send_time: String,

    /// Contributor's feature-branch tip (the `t4.md` commit). The `c` tag
    /// on the PR event must equal this (assertion 3).
    pr_tip_oid: String,

    /// Feature branch name (always `BRANCH`). The `branch-name` tag on the
    /// PR event must equal this (assertion 5).
    branch_name: String,

    /// Expected clone URL for the contributor's GRASP-06 `/prs/` endpoint on
    /// the user grasp server:
    /// `http://127.0.0.1:<user_grasp_port>/prs/<contributor_npub>/<IDENTIFIER>.git`
    ///
    /// The GRASP-06 fallback path in `push.rs` builds this from the server
    /// URL, contributor npub, and identifier. We replicate that construction
    /// here independently so the assertion is not tautological against ngit's
    /// own URL-building logic.
    ///
    /// The URL uses the contributor's **bech32 npub** (not hex) in the path,
    /// matching the GRASP-06 spec (`/persistent/clones/grasp/06.md` §
    /// "Git Smart HTTP Service"): `GET /prs/<npub>/<percent-encoded-id>.git`.
    user_grasp_prs_clone_url: String,

    /// OID that `refs/nostr/<pr_event_id>` resolves to inside the user
    /// grasp's bare repo at
    /// `<git_data_path>/prs/<contributor_pubkey_hex>/<IDENTIFIER>.git`.
    ///
    /// The on-disk path uses **lowercase hex** (not npub) per
    /// `ngit-grasp/src/grasp06/paths.rs::prs_repo_path`. Must equal
    /// `pr_tip_oid` (assertion 7).
    user_grasp_pr_ref_oid: String,

    /// Total count of kind-30617 (Kind::GitRepoAnnouncement) events
    /// authored by the contributor across the default relay and the
    /// user_grasp relay. Must equal 0 (assertion 9): the GRASP-06 fallback
    /// does not create a personal-fork announcement.
    repo_announcement_count_contributor: usize,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: run [`capture_snapshot`] exactly once per test binary and
/// hand each test case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("send_pr_all_server_down_all fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- Harness: one vanilla relay + one repo grasp + one GRASP-06 user grasp
    //
    // The user_grasp server is the contributor's personal grasp, started with
    // NGIT_GRASP06_ENABLE=true so it serves the /prs/ endpoint. It is NOT
    // listed in the repo announcement — only the (soon-to-be-killed) "repo"
    // grasp is. Both servers appear in `NGIT_GRASP_DEFAULT_SET` (the harness
    // always aggregates all grasps), but that env var is only consulted in the
    // interactive fallback path, which is never reached in non-interactive mode
    // when the user grasp list is non-empty.
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_grasp_server_grasp06("user_grasp")
    .build()
    .await?;

    // --- 1. Maintainer publishes the repo with only the "repo" grasp ---------
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("pr-all-down maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 2. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "pr-all-down contributor".into(),
            },
        )
        .await?;

    // Extract contributor keys so we can sign the user grasp list event and
    // later query the relay for their authored events.
    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after AsContributor login")?;
    let contributor_keys =
        Keys::parse(&contributor_nsec).context("contributor nostr.nsec is not a valid key")?;
    let contributor_pubkey = contributor_keys.public_key();
    let contributor_npub = contributor_pubkey
        .to_bech32()
        .context("failed to bech32-encode contributor pubkey")?;
    // Lowercase 64-char hex — used for the on-disk prs/<hex>/<id>.git path.
    // ngit-grasp stores the submitter pubkey as hex on disk even though the
    // HTTP path uses npub; see ngit-grasp/src/grasp06/paths.rs::prs_repo_path.
    let contributor_pubkey_hex = contributor_pubkey.to_hex();

    // --- 3. Publish the contributor's user grasp list ------------------------
    //
    // Points at `ws://127.0.0.1:<user_grasp_port>`. When ngit send runs and
    // all repo servers are down, `push.rs` reads the user's grasp list and
    // pushes to the GRASP-06 /prs/ endpoint on those servers.
    harness
        .publish_user_grasp_list(&contributor_keys, &["user_grasp"])
        .await
        .context("failed to publish contributor's user grasp list")?;

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

    // Pre-condition: all three OIDs must be distinct.
    if merge_base_oid == main_tip_at_send_time {
        bail!("arrange bug: merge_base_oid == main_tip_at_send_time ({merge_base_oid})");
    }
    if merge_base_oid == pr_tip_oid {
        bail!("arrange bug: merge_base_oid == pr_tip_oid ({merge_base_oid})");
    }
    if main_tip_at_send_time == pr_tip_oid {
        bail!("arrange bug: main_tip_at_send_time == pr_tip_oid ({main_tip_at_send_time})");
    }

    // --- 7. Build expected GRASP-06 /prs/ clone URL --------------------------
    //
    // The GRASP-06 fallback in `push.rs` will construct the clone URL as
    // `http://<host>:<port>/prs/<contributor-npub>/<identifier>.git`.
    //
    // Per the GRASP-06 spec (/persistent/clones/grasp/06.md § "Git Smart HTTP
    // Service"), the URL path uses the contributor's **npub** (bech32), NOT
    // hex. The on-disk layout uses hex (see ngit-grasp/src/grasp06/paths.rs)
    // but the HTTP-visible path and the `clone` tag always carry the npub.
    //
    // We construct this string by hand so the assertion is independent of
    // ngit's own URL-building helpers — a tautological test would catch nothing.
    let user_grasp_http_url = harness.grasp("user_grasp").url().to_string(); // "http://127.0.0.1:PORT"
    let user_grasp_prs_clone_url = format!(
        "{}/prs/{}/{}.git",
        user_grasp_http_url.trim_end_matches('/'),
        contributor_npub,
        IDENTIFIER,
    );

    // --- 8. Take the only repo grasp offline ---------------------------------
    //
    // After this drop, ALL servers in the repo announcement are dead. The
    // contributor's env snapshot (captured at clone time) still lists the now-
    // dead server in `NGIT_GRASP_DEFAULT_SET`, but `push.rs` will exhaust the
    // repo announcement servers first, then fall back to the user grasp list.
    let repo_grasp = harness
        .take_grasp("repo")
        .context("repo grasp was never registered or already taken")?;
    let dead_url = repo_grasp.url().to_string();
    drop(repo_grasp);

    // Sanity: confirm the repo grasp is actually unreachable.
    let probe = tokio::net::TcpStream::connect(dead_url.trim_start_matches("http://")).await;
    assert!(
        probe.is_err(),
        "repo grasp should be unreachable after drop, but TCP connect to \
         {dead_url} succeeded — cannot test all-servers-down path",
    );

    // --- 9. Contributor: ngit send --force-pr --------------------------------
    //
    // Non-interactive (`--force-pr` plus explicit title/description). Because
    // the contributor has a non-empty user grasp list pointing at a GRASP-06
    // server, push.rs pushes to /prs/<npub>/<id>.git on that server and
    // includes the resulting URL as the sole `clone` tag. No kind-30617 is
    // created — the personal-fork code path is not taken.
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

    // --- 10. Capture events from live surfaces -------------------------------
    //
    // send.rs publishes PR events to `user_ref.relays.write()` (default relay)
    // and `repo_ref.relays` (the dead repo grasp — fails silently). The
    // user_grasp relay is not in repo_ref.relays, so the PR event is only
    // on the default relay.
    let pr_events_default = harness
        .relay("default")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count_default = pr_events_default.len();
    let pr_event = pr_events_default
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on default relay after `ngit send --force-pr`",
        )?;

    // KIND_PULL_REQUEST_UPDATE events across all live surfaces.
    let pr_updates_default = harness
        .relay("default")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_updates_user_grasp = harness
        .grasp("user_grasp")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_update_count = pr_updates_default.len() + pr_updates_user_grasp.len();

    // --- 11. Read git ref from user grasp bare repo --------------------------
    //
    // The GRASP-06 on-disk layout is `<git_data_path>/prs/<hex>/<id>.git`
    // (ngit-grasp/src/grasp06/paths.rs::prs_repo_path). The hex is the
    // lowercase 64-char pubkey, NOT the npub that appears in the HTTP URL.
    let pr_event_id_hex = pr_event.id.to_hex();
    let user_grasp_pr_ref_oid = harness
        .grasp("user_grasp")
        .read_nostr_ref_prs(&contributor_pubkey_hex, IDENTIFIER, &pr_event_id_hex)
        .await?;

    // --- 12. Count any kind-30617 announcements by the contributor -----------
    //
    // After the GRASP-06 fallback, the contributor must NOT have published a
    // personal-fork kind-30617. Query both the default relay and the user_grasp
    // relay (which doubles as a standard NIP-01 relay over its relay_url()).
    let announcements_default = harness
        .relay("default")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(Kind::Custom(30617)),
        )
        .await?;
    let announcements_user_grasp = harness
        .grasp("user_grasp")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(Kind::Custom(30617)),
        )
        .await?;
    let repo_announcement_count_contributor =
        announcements_default.len() + announcements_user_grasp.len();

    Ok(Snapshot {
        pr_event,
        pr_count_default,
        pr_update_count,
        maintainer_pubkey,
        identifier: IDENTIFIER.to_string(),
        merge_base_oid,
        main_tip_at_send_time,
        pr_tip_oid,
        branch_name: BRANCH.to_string(),
        user_grasp_prs_clone_url,
        user_grasp_pr_ref_oid,
        repo_announcement_count_contributor,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST event is published by the
/// contributor on the default relay.
///
/// send.rs publishes PR events to `user_ref.relays.write()` which includes
/// the default relay. A count > 1 would indicate a duplicate-publish bug.
#[rstest]
#[tokio::test]
async fn pr_event_exactly_one(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count_default, 1,
        "expected exactly one KIND_PULL_REQUEST event on default relay authored by \
         contributor; got {}",
        s.pr_count_default,
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

/// Assertion 3: the PR event's `c` tag equals the contributor's
/// feature-branch tip OID.
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

/// Assertion 4: the PR event's `merge-base` tag equals the OID the
/// contributor branched from, NOT the current `main` tip.
#[rstest]
#[tokio::test]
async fn merge_base_tag_is_fork_point(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "merge-base").as_deref(),
        Some(s.merge_base_oid.as_str()),
        "PR event `merge-base` tag should equal the contributor's fork-point OID;\n\
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

/// Assertion 6: the PR event has exactly one `clone` tag URL, pointing at
/// the GRASP-06 `/prs/<contributor-npub>/<identifier>.git` endpoint on the
/// user grasp server.
///
/// When all repo servers are offline, the GRASP-06 fallback path (push.rs)
/// pushes git data to the user grasp server's `/prs/` space and records the
/// resulting URL as the sole `clone` tag on the PR event. The URL path uses
/// the contributor's **npub** per the GRASP-06 spec
/// (`/persistent/clones/grasp/06.md` § "Git Smart HTTP Service").
#[rstest]
#[tokio::test]
async fn clone_tag_is_user_grasp_prs_url(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
        clone_tag_urls[0], s.user_grasp_prs_clone_url,
        "PR event's `clone` URL should be the GRASP-06 /prs/ endpoint on the \
         user grasp server;\ngot:  {:?}\nwant: {:?}",
        clone_tag_urls[0], s.user_grasp_prs_clone_url,
    );
    Ok(())
}

/// Assertion 7: the user grasp server received the git data push — its GRASP-06
/// bare repo at `<git_data_path>/prs/<contributor-hex>/<identifier>.git`
/// (ngit-grasp/src/grasp06/paths.rs::prs_repo_path) has a
/// `refs/nostr/<pr_event_id>` ref resolving to the contributor's tip OID.
///
/// The on-disk path uses the lowercase hex of the contributor's pubkey, NOT the
/// npub that appears in the HTTP URL — this asymmetry is intentional per the
/// ngit-grasp design; see paths.rs for rationale.
#[rstest]
#[tokio::test]
async fn user_grasp_has_pr_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.user_grasp_pr_ref_oid, s.pr_tip_oid,
        "user grasp GRASP-06 repo: refs/nostr/<pr_event_id> should resolve to pr_tip_oid; \
         got {:?}, want {:?}",
        s.user_grasp_pr_ref_oid, s.pr_tip_oid,
    );
    Ok(())
}

/// Assertion 8: no KIND_PULL_REQUEST_UPDATE event was emitted on any live
/// relay surface (default relay or user grasp relay).
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

/// Assertion 9: no kind-30617 (GitRepoAnnouncement) was emitted by the
/// contributor on any live relay surface.
///
/// The GRASP-06 fallback pushes git data to `/prs/<npub>/<id>.git` directly
/// and does not create a personal-fork announcement. Any regression that
/// re-enables the personal-fork code path will publish a kind-30617 and fail
/// here.
#[rstest]
#[tokio::test]
async fn no_personal_fork_announcement(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.repo_announcement_count_contributor, 0,
        "expected zero kind-30617 events by the contributor across all live relay surfaces; \
         got {} — a personal-fork was emitted, but the GRASP-06 fallback should not \
         produce a kind-30617",
        s.repo_announcement_count_contributor,
    );
    Ok(())
}
