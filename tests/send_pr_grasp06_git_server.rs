//! End-to-end coverage of `ngit send --force-pr --git-server <url>` where the
//! supplied git server is a **GRASP-06-enabled** grasp server whose base URL is
//! given as `ws://127.0.0.1:<port>` — the form a user would type when they know
//! only the server's domain (e.g. `--git-server ws://relay.example.com`).
//!
//! ## What this test covers
//!
//! `select_servers_push_refs_and_generate_pr_or_pr_update_event`
//! (push.rs:428-448) inspects the `--git-server` value. When it does **not**
//! end with `.git` and does **not** start with `git@`,
//! `user_server_is_direct_url` is `false` and the server is treated as a GRASP
//! base URL. `format_grasp_server_url_as_grasp06_prs_url` is called, converting
//! `ws://127.0.0.1:PORT` →
//! `http://127.0.0.1:PORT/prs/<contributor-npub>/<identifier>.git`, and that
//! GRASP-06 endpoint is prepended to `to_try`.
//!
//! The test then verifies that:
//!
//! 1. The PR event's `clone` tag carries the GRASP-06 `/prs/` URL on the
//!    user-specified server, not any regular grasp clone URL.
//! 2. All three servers — the GRASP-06 server and both announcement grasps —
//!    received the git data push.
//! 3. The GRASP-06 server's base URL does not appear in the repo announcement
//!    `clone` tag (it was never passed to `ngit init`).
//! 4. No KIND_PULL_REQUEST_UPDATE was emitted.
//!
//! ## Arrangement
//!
//! Steps 1–6 mirror `send_pr_vanilla_git_server.rs` exactly so the two tests
//! can be read side by side; the only departure is the `--git-server` value
//! (ws:// URL instead of a `.git` URL) and the extra server type (GRASP-06
//! instead of a vanilla git server).
//!
//! 1. Harness: one relay (`"default"`), two standard grasps (`"repo"` and
//!    `"repo_secondary"`), one GRASP-06 grasp (`"extra_grasp"`).
//! 2. Maintainer publishes the repo with **both** standard grasps in the
//!    announcement. The GRASP-06 server is **not** mentioned.
//! 3. A fresh contributor clones and checks out the `"feature"` branch.
//! 4. Contributor commits `t3.md` — establishes the fork point.
//! 5. **Maintainer advances `main`** so `merge_base_oid ≠
//!    main_tip_at_send_time`.
//! 6. Contributor commits `t4.md`.
//! 7. Contributor runs `ngit send HEAD~2 --force-pr --title … --description …
//!    --git-server ws://127.0.0.1:<extra_grasp_port>`.
//! 8. [`capture_snapshot`] reads all events and git refs; harness drops.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event is published.
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's feature-branch tip OID.
//! 4. The `merge-base` tag equals the fork point.
//! 5. The `branch-name` tag equals `"feature"`.
//! 6. The PR event's `clone` tag has exactly one URL equal to the GRASP-06
//!    `/prs/<contributor-npub>/<identifier>.git` endpoint on `extra_grasp`.
//! 7. All three servers — both standard grasps and the GRASP-06 extra server —
//!    have a `refs/nostr/<event_id>` ref resolving to the contributor's tip
//!    OID.
//! 8. The GRASP-06 server's URL does NOT appear in the repo announcement's
//!    `clone` tag.
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

/// Identifier passed to `ngit init --identifier`. Distinct from all other
/// send-PR tests to prevent cross-test relay pollution.
const IDENTIFIER: &str = "pr-grasp06-git-server-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

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
    /// Must match the `merge-base` tag (assertion 4).
    merge_base_oid: String,

    /// OID of `main` after the maintainer's "advance main" push. Used only
    /// in the pre-condition check.
    main_tip_at_send_time: String,

    /// Contributor's feature-branch tip (the `t4.md` commit). Must match the
    /// `c` tag (assertion 3) and the `refs/nostr/<event_id>` OID on all three
    /// servers (assertion 7).
    pr_tip_oid: String,

    /// Feature branch name (always `BRANCH`). Must match `branch-name` tag
    /// (assertion 5).
    branch_name: String,

    /// The GRASP-06 `/prs/` URL that ngit should have placed in the PR
    /// event's `clone` tag:
    /// `http://127.0.0.1:<extra_grasp_port>/prs/<contributor-npub>/<IDENTIFIER>.git`
    ///
    /// Constructed independently of ngit's own
    /// `format_grasp_server_url_as_grasp06_prs_url` so the assertion is not
    /// tautological.
    extra_grasp_prs_clone_url: String,

    /// Base URL of the GRASP-06 server (`http://127.0.0.1:<port>`). Used in
    /// assertion 8 to verify it is absent from the repo announcement clone
    /// tags.
    extra_grasp_http_base: String,

    /// All `clone` tag values from the kind-30617 repo announcement. None of
    /// them must contain the extra grasp's base URL (assertion 8).
    announcement_clone_urls: Vec<String>,

    /// OID at `refs/nostr/<pr_event_id>` in the primary grasp's bare repo
    /// (`<git_data_path>/<maintainer_npub>/<IDENTIFIER>.git`).
    /// Must equal `pr_tip_oid` (assertion 7).
    grasp_primary_pr_ref_oid: String,

    /// OID at `refs/nostr/<pr_event_id>` in the secondary grasp's bare repo
    /// (`<git_data_path>/<maintainer_npub>/<IDENTIFIER>.git`).
    /// Must equal `pr_tip_oid` (assertion 7).
    grasp_secondary_pr_ref_oid: String,

    /// OID at `refs/nostr/<pr_event_id>` in the GRASP-06 server's bare repo
    /// at `<git_data_path>/prs/<contributor_pubkey_hex>/<IDENTIFIER>.git`.
    /// Must equal `pr_tip_oid` (assertion 7).
    extra_grasp_pr_ref_oid: String,
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
                    .expect("send_pr_grasp06_git_server fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- Harness: vanilla relay + two standard grasps + one GRASP-06 grasp --
    //
    // The GRASP-06 server (`extra_grasp`) is deliberately NOT included in the
    // repo announcement — the test verifies ngit routes the PR to its /prs/
    // endpoint solely because the user passed its ws:// base URL via
    // `--git-server`, and that the announcement grasps also receive the push.
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_grasp_server("repo_secondary")
    .with_grasp_server_grasp06("extra_grasp")
    .build()
    .await?;

    // ws:// URL the contributor passes to `--git-server`. This is how a real
    // user would specify a grasp server they know by domain — just the base
    // relay URL, not a full clone path. push.rs detects that it doesn't end
    // with `.git` and is not an SSH URL (`user_server_is_direct_url = false`),
    // so it calls `format_grasp_server_url_as_grasp06_prs_url` to build the
    // GRASP-06 endpoint before prepending it to `to_try`.
    let extra_grasp_ws_url = harness.grasp("extra_grasp").relay_url(); // "ws://127.0.0.1:PORT"
    let extra_grasp_http_base = harness.grasp("extra_grasp").url().to_string(); // "http://127.0.0.1:PORT"

    // --- 1. Maintainer publishes the repo with both standard grasps ----------
    //
    // The GRASP-06 server is deliberately NOT included here.
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("pr-grasp06-test maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            additional_grasp_roles: vec!["repo_secondary".into()],
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();
    let maintainer_npub = maintainer_pubkey
        .to_bech32()
        .context("failed to bech32-encode maintainer pubkey")?;

    // --- 2. Extract announcement clone URLs (before any extra-server push) ---
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
                display_name: "pr-grasp06-test contributor".into(),
            },
        )
        .await?;

    // Contributor pubkey — needed to build the expected /prs/ URL and to
    // locate the GRASP-06 bare repo on disk.
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
    // Lowercase hex — used for the on-disk `prs/<hex>/<id>.git` path.
    // ngit-grasp stores the submitter pubkey in hex on disk even though the
    // HTTP path uses npub; see ngit-grasp/src/grasp06/paths.rs::prs_repo_path.
    let contributor_pubkey_hex = contributor_pubkey.to_hex();

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

    // --- 7. Build expected GRASP-06 /prs/ clone URL --------------------------
    //
    // push.rs calls `format_grasp_server_url_as_grasp06_prs_url(ws_url, pk, id)`
    // which normalises ws:// → http:// and produces:
    // `http://<host>:<port>/prs/<contributor-npub>/<identifier>.git`
    //
    // We replicate that construction here independently — using the http base
    // we already have — so the assertion is not tautological against ngit's own
    // URL building helpers.
    let extra_grasp_prs_clone_url = format!(
        "{}/prs/{}/{}.git",
        extra_grasp_http_base.trim_end_matches('/'),
        contributor_npub,
        IDENTIFIER,
    );

    // --- 8. Contributor: ngit send --force-pr --git-server <ws_url> ----------
    //
    // `ws://127.0.0.1:<port>` does NOT end with `.git` and does NOT start with
    // `git@`, so `user_server_is_direct_url` (push.rs:430-432) is `false`.
    // push.rs calls `format_grasp_server_url_as_grasp06_prs_url` and prepends
    // the resulting /prs/ URL to `to_try` ahead of the repo's own grasps.
    // The PR event is generated against that first URL, which therefore becomes
    // the sole `clone` tag value; then the loop continues and pushes the same
    // git data to the two announcement grasps.
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
            &extra_grasp_ws_url,
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-pr --git-server (GRASP-06)")?;
    if !send_out.status.success() {
        bail!(
            "ngit send --force-pr --git-server (GRASP-06) exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            send_out.status,
            String::from_utf8_lossy(&send_out.stdout),
            String::from_utf8_lossy(&send_out.stderr),
        );
    }

    // --- 9. Capture events from all surfaces ---------------------------------

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
             found on primary grasp after `ngit send --force-pr --git-server` (GRASP-06)",
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

    // --- 10. Read git refs from both standard grasps and the GRASP-06 server -
    //
    // Standard grasps store repos at `<git_data_path>/<maintainer_npub>/<id>.git`
    // (layout imposed by ngit-grasp's announcement handler).
    //
    // The GRASP-06 server stores the push at
    // `<git_data_path>/prs/<contributor_pubkey_hex>/<id>.git` (ngit-grasp
    // /src/grasp06/paths.rs::prs_repo_path). Note: hex on disk, npub in URL.
    let pr_event_id_hex = pr_event.id.to_hex();

    let grasp_primary_pr_ref_oid = harness
        .grasp("repo")
        .read_nostr_ref(&maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    let grasp_secondary_pr_ref_oid = harness
        .grasp("repo_secondary")
        .read_nostr_ref(&maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    let extra_grasp_pr_ref_oid = harness
        .grasp("extra_grasp")
        .read_nostr_ref_prs(&contributor_pubkey_hex, IDENTIFIER, &pr_event_id_hex)
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
        extra_grasp_prs_clone_url,
        extra_grasp_http_base,
        announcement_clone_urls,
        grasp_primary_pr_ref_oid,
        grasp_secondary_pr_ref_oid,
        extra_grasp_pr_ref_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST event is published by the
/// contributor on the primary grasp.
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

/// Assertion 2: the `a` tag encodes the maintainer's repo coordinate.
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

/// Assertion 3: the `c` tag equals the contributor's feature-branch tip OID.
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

/// Assertion 4: the `merge-base` tag equals the fork point, not `main` tip.
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

/// Assertion 5: the `branch-name` tag equals `"feature"`.
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

/// Assertion 6: the PR event's `clone` tag has exactly one URL and it is the
/// GRASP-06 `/prs/<contributor-npub>/<identifier>.git` endpoint on the
/// user-specified GRASP-06 server.
///
/// Because `user_server_is_direct_url` is `false` when the value doesn't end
/// with `.git` (push.rs:430-432), the ws:// URL is passed through
/// `format_grasp_server_url_as_grasp06_prs_url` (push.rs:441-446). The
/// resulting `/prs/` URL enters `to_try` first (push.rs:446),
/// `push_refs_and_generate_pr_or_pr_update_event` generates the unsigned PR
/// event on that first server and reuses it for all subsequent servers
/// (push.rs:638-655), so the GRASP-06 URL is the sole `clone` value.
#[rstest]
#[tokio::test]
async fn clone_tag_is_grasp06_prs_url(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
        clone_tag_urls[0], s.extra_grasp_prs_clone_url,
        "PR event's `clone` URL should be the GRASP-06 /prs/ endpoint on the \
         user-specified server (not a standard grasp clone URL);\n\
         got:  {:?}\nwant: {:?}",
        clone_tag_urls[0], s.extra_grasp_prs_clone_url,
    );
    Ok(())
}

/// Assertion 7: all three servers received the git data push — both standard
/// grasps and the GRASP-06 extra server each have `refs/nostr/<pr_event_id>`
/// resolving to the contributor's tip OID.
///
/// The loop in `push_refs_and_generate_pr_or_pr_update_event` (push.rs:637)
/// iterates every entry in `to_try`, which is
/// `[grasp06_prs_url, grasp_primary, grasp_secondary]`. The GRASP-06 /prs/
/// URL is recognised as a grasp clone URL by `is_grasp_server_clone_url`
/// (contains npub in path) and routed through `push_to_remote_url`, so ngit-
/// grasp stores the result at `prs/<hex>/<id>.git`. The standard grasps are
/// also recognised and land at `<maintainer_npub>/<id>.git` as normal.
#[rstest]
#[tokio::test]
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
        s.extra_grasp_pr_ref_oid, s.pr_tip_oid,
        "GRASP-06 extra grasp: refs/nostr/<pr_event_id> in prs/<hex>/<id>.git should \
         resolve to pr_tip_oid; got {:?}, want {:?}",
        s.extra_grasp_pr_ref_oid, s.pr_tip_oid,
    );
    Ok(())
}

/// Assertion 8: the GRASP-06 server's base URL does not appear in the repo
/// announcement's `clone` tag.
///
/// The server was supplied at send-time via `--git-server`; it was never
/// passed to `ngit init --grasp-server` and is therefore not part of the
/// kind-30617 announcement. This assertion guards against a regression where
/// the user-supplied server was accidentally injected into the announcement.
#[rstest]
#[tokio::test]
async fn extra_grasp_not_in_announcement(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    for url in &s.announcement_clone_urls {
        assert!(
            !url.contains(&s.extra_grasp_http_base),
            "GRASP-06 extra server base URL ({:?}) should not appear in the repo \
             announcement's clone tag, but found it in {url:?}",
            s.extra_grasp_http_base,
        );
    }
    Ok(())
}

/// Assertion 9: no KIND_PULL_REQUEST_UPDATE event was emitted on any surface.
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
