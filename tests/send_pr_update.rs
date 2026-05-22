//! End-to-end coverage of `ngit send --in-reply-to` (PR Update path) against
//! a repo whose kind-30617 announcement lists **two** grasp servers.
//!
//! ## What this test covers
//!
//! `generate_unsigned_pr_or_update_event` (git_events.rs:520–644) has two
//! branches that share most tag-building logic but diverge at the event kind
//! and at the extra tags that are specific to each case:
//!
//! * **New PR** (kind 1618): `subject`, `alt`, `branch-name`, `e`/`p` tags.
//! * **PR Update** (kind 1619): uppercase `E` and `P` tags pointing at the
//!   *original* PR event, no `branch-name`, no `subject`.
//!
//! This file covers the PR Update path.  The sibling file `send_pr.rs`
//! covers the original-PR path and verifies that a first `ngit send
//! --force-pr` produces no update event.
//!
//! ## Arrangement
//!
//! Steps 1–6 mirror `send_pr.rs` exactly so the two tests can be read side by
//! side; step 7 onward is the update-specific part.
//!
//! 1. Harness: one vanilla relay (`"default"`), two grasp servers (`"repo"` and
//!    `"repo_secondary"`).
//! 2. Maintainer publishes repo with both grasps in the announcement.
//! 3. Fresh contributor clones and checks out the `"feature"` branch.
//! 4. Contributor commits `t3.md` (first PR commit; its parent is the fork
//!    point = `merge_base_oid`).
//! 5. **Maintainer advances `main`** so `merge_base_oid ≠
//!    main_tip_at_send_time`.
//! 6. Contributor commits `t4.md` then runs `ngit send HEAD~2 --force-pr`. This
//!    is the **original PR** — captured for its event ID.
//! 7. Contributor commits `t5.md` (the update commit).
//! 8. Contributor runs `ngit send HEAD~3 --in-reply-to <pr_event_id_hex>`. This
//!    is the **PR Update** — the act under test.
//! 9. [`capture_snapshot`] reads all events and git refs; harness drops. Each
//!    `#[rstest]` case asserts on one slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST_UPDATE event is published (primary grasp).
//! 2. The `a` tag is the canonical 30617 coordinate for the maintainer's repo.
//! 3. The `c` tag equals the contributor's updated tip OID (`t5.md` commit).
//! 4. The uppercase `E` tag equals the original PR event's ID (hex).
//! 5. The uppercase `P` tag equals the original PR author's pubkey
//!    (contributor).
//! 6. The `merge-base` tag equals the unchanged fork point (same as original
//!    PR).
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

/// Identifier passed to `ngit init --identifier`. Distinct from `send_pr.rs`
/// (`"pr-test-repo"`) so test runs on a shared vanilla-relay surface cannot
/// see each other's events.
const IDENTIFIER: &str = "pr-update-test-repo";

/// Feature branch name the contributor checks out before committing.
const BRANCH: &str = "feature";

/// `KIND_PULL_REQUEST` (kind 1618). Mirrored from `src/lib/git_events.rs` so
/// the test crate does not have to depend on the ngit lib crate.
const KIND_PULL_REQUEST: Kind = Kind::Custom(1618);

/// `KIND_PULL_REQUEST_UPDATE` (kind 1619). Mirrored for the same reason.
const KIND_PULL_REQUEST_UPDATE: Kind = Kind::Custom(1619);

// ---------------------------------------------------------------------------
// Snapshot — all observable side-effects captured once and shared
// ---------------------------------------------------------------------------

/// Everything observable after the two-step arrangement (original PR send
/// followed by PR update send), captured during [`capture_snapshot`] and
/// shared read-only across the eight `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// The KIND_PULL_REQUEST_UPDATE event published by the contributor,
    /// read from the primary grasp. Assertions 2–6 read from here.
    pr_update_event: Event,

    /// Number of KIND_PULL_REQUEST_UPDATE events authored by the contributor
    /// on the primary grasp. Must equal 1 (assertion 1).
    pr_update_count_primary: usize,

    /// Number of KIND_PULL_REQUEST events authored by the contributor on the
    /// primary grasp after both sends. Must still equal 1 (assertion 8): the
    /// update send must not have accidentally published a new PR event.
    pr_count_primary: usize,

    /// Hex-encoded event ID of the original KIND_PULL_REQUEST event. The
    /// uppercase `E` tag on the PR update event must equal this (assertion 4).
    original_pr_event_id: String,

    /// Public key of the contributor — the author of the original PR event.
    /// The uppercase `P` tag on the PR update event must equal this
    /// (assertion 5).
    contributor_pubkey: PublicKey,

    /// OID of the contributor's feature-branch tip after committing `t5.md`.
    /// The `c` tag on the PR update event must equal this (assertion 3).
    update_tip_oid: String,

    /// OID the contributor branched off from — the parent of the first PR
    /// commit (`t3.md`), equal to `published.initial_oid`. The `merge-base`
    /// tag on the PR update event must equal this (assertion 6), confirming
    /// that no rebase occurred between the original send and the update.
    merge_base_oid: String,

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
                    .expect("send_pr_update fixture: capture_snapshot failed"),
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
            display_name: Some("pr-update maintainer".into()),
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
                display_name: "pr-update contributor".into(),
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
    // merge_base_oid that must appear in both the original PR and the update.
    run_git(&contributor, &["checkout", "-b", BRANCH]).await?;
    std::fs::write(contributor.dir().join("t3.md"), "some content\n")
        .context("failed to write t3.md in contributor clone")?;
    run_git(&contributor, &["add", "t3.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t3.md", "--no-gpg-sign"],
    )
    .await?;

    let merge_base_oid = published.initial_oid.clone();

    // --- 4. Maintainer: advance main -----------------------------------------
    //
    // Same rationale as send_pr.rs step 5: creates a gap between
    // `merge_base_oid` and the current `main` tip so the merge-base
    // assertion is non-trivial.
    std::fs::write(publisher.dir().join("t-on-main.md"), "content\n")
        .context("failed to write t-on-main.md on publisher side")?;
    run_git(&publisher, &["add", "t-on-main.md"]).await?;
    run_git(
        &publisher,
        &["commit", "-m", "advance main", "--no-gpg-sign"],
    )
    .await?;
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("maintainer nostr_push to advance main failed")?;

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
    // Establishes the original KIND_PULL_REQUEST event whose ID will be passed
    // to `--in-reply-to` in the update step. `--force-pr` ensures PR kind
    // regardless of commit payload size (same as send_pr.rs).
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

    // Capture the original PR event from the primary grasp so we have its ID.
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

    // --- 7. Contributor: third commit (t5.md) — the update commit -------------
    std::fs::write(contributor.dir().join("t5.md"), "more content\n")
        .context("failed to write t5.md in contributor clone")?;
    run_git(&contributor, &["add", "t5.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t5.md", "--no-gpg-sign"],
    )
    .await?;
    let update_tip_oid = git_rev_parse(&contributor, "HEAD").await?;

    // --- 8. Contributor: ngit send --in-reply-to (PR Update) -----------------
    //
    // `HEAD~3` covers all three feature-branch commits (t3, t4, t5) above the
    // fork point. `--in-reply-to <hex>` identifies the original PR. The
    // production code (`send.rs:237-238`) auto-detects `as_pr = true` because
    // the original event is kind 1618.
    let send_update_out = contributor
        .ngit([
            "send",
            "HEAD~3",
            "--in-reply-to",
            &original_pr_event_id,
            "--title",
            "update: add t5",
            "--description",
            "adds t5.md to the feature",
        ])
        .output()
        .await
        .context("failed to spawn ngit send --in-reply-to (PR update)")?;
    if !send_update_out.status.success() {
        bail!(
            "ngit send --in-reply-to exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            send_update_out.status,
            String::from_utf8_lossy(&send_update_out.stdout),
            String::from_utf8_lossy(&send_update_out.stderr),
        );
    }

    // --- 9. Capture events from all surfaces ----------------------------------

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
             after `ngit send --in-reply-to`",
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

    // --- 10. Read git refs from both grasps before harness drops --------------
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
        merge_base_oid,
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
/// The coordinate format is `"30617:<pubkey-hex>:<identifier>"` per NIP-01.
/// An incorrect pubkey would mean ngit used the wrong announcement; an
/// incorrect identifier would mean the identifier round-tripped incorrectly.
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
/// feature-branch tip OID after adding `t5.md`.
///
/// The `c` tag is what `get_commit_id_from_patch` (git_events.rs:58-60)
/// reads to locate the tip commit for `ngit pr checkout` / `ngit pr apply`.
/// An incorrect value would produce the wrong working tree after checkout.
#[rstest]
#[tokio::test]
async fn c_tag_is_update_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "c").as_deref(),
        Some(s.update_tip_oid.as_str()),
        "PR update event `c` tag should equal contributor's updated tip OID; \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "c"),
        s.update_tip_oid,
    );
    Ok(())
}

/// Assertion 4: the PR update event's uppercase `E` tag equals the original
/// PR event's ID (hex).
///
/// The uppercase `E` tag is the NIP-10 root-event marker written by
/// `pr_update_specific_tags` (git_events.rs:527-529). It is how review tools
/// correlate the update back to the original proposal thread. An incorrect or
/// missing `E` tag would silently orphan the update from its parent PR.
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
/// The uppercase `P` tag is written by `pr_update_specific_tags`
/// (git_events.rs:530-533) using `root_proposal.pubkey`. Since the
/// contributor published the original PR, their pubkey must appear here.
/// An incorrect key (e.g. maintainer's key) would break notification routing
/// in clients that use `P` to identify the original thread's author.
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

/// Assertion 6: the PR update event's `merge-base` tag equals the unchanged
/// fork point — the same `merge_base_oid` as the original PR.
///
/// `select_servers_push_refs_and_generate_pr_or_pr_update_event` passes
/// `git_repo.get_commit_parent(first_commit)` as the merge_base for both
/// new PRs and updates. Because the contributor did NOT rebase between the
/// original send and this update, the first commit in the range is still
/// `t3.md` and its parent is still `published.initial_oid`.
///
/// A regression that substitutes `main` tip or `HEAD` for the parent of the
/// first commit would fail here because `merge_base_oid` ≠ the current
/// `main` tip (the maintainer advanced main in step 4).
#[rstest]
#[tokio::test]
async fn merge_base_tag_unchanged(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "merge-base").as_deref(),
        Some(s.merge_base_oid.as_str()),
        "PR update event `merge-base` tag should equal the original fork-point OID \
         (no rebase occurred); got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "merge-base"),
        s.merge_base_oid,
    );
    Ok(())
}

/// Assertion 7: both grasp servers received the git data push — each bare
/// repo has a `refs/nostr/<update_event_id>` ref resolving to the updated
/// tip OID.
///
/// Same redundancy property as assertion 7 in `send_pr.rs`: a user running
/// two grasps must be able to fetch the updated commits from either one
/// independently. A premature `break` or changed iteration order in
/// `push_refs_and_generate_pr_or_pr_update_event` (push.rs:736-753) would
/// silently leave one grasp stale.
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

/// Assertion 8: no new KIND_PULL_REQUEST event was published by the contributor
/// on the primary grasp — the original PR event count is still exactly one.
///
/// `ngit send --in-reply-to` routes through
/// `select_servers_push_refs_and_generate_pr_or_pr_update_event` with
/// `root_proposal.is_some()`, which produces kind 1619 (not 1618). A
/// regression that falls through to the wrong branch or incorrectly detects
/// `as_pr = false` could publish a new kind-1618 event instead of a kind-1619
/// update.
#[rstest]
#[tokio::test]
async fn no_new_pr_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count_primary, 1,
        "expected exactly one KIND_PULL_REQUEST on primary grasp after both sends; \
         got {} — did the update accidentally publish a new PR?",
        s.pr_count_primary,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — identical to send_pr.rs copies; kept local to avoid cross-test
// coupling. If a third test file needs these, extract to test_harness.
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
/// repository at `bare_repo`. Returns an error if the ref is absent (push
/// never landed).
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
/// Used to locate the specific PR event for this test's branch among any
/// events the contributor may have published across test runs on shared relay
/// surfaces.
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
