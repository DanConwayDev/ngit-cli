//! Coverage for the "no-GRASP repo → patch kind" fallback.
//!
//! When `git push origin pr/<branch>` is run against a repo whose
//! kind-30617 announcement lists **no** GRASP server (only vanilla
//! git-server clone URLs), the push must produce `Kind::GitPatch`
//! events (the traditional patch-kind proposal format), not
//! `KIND_PULL_REQUEST` (kind 1618).
//!
//! The load-bearing condition in `push.rs:648-652` is:
//!
//! ```text
//! let repo_has_grasp_server = !repo_ref.grasp_servers().is_empty();
//! let use_pr_kind = ... || (root_proposal.is_none() && repo_has_grasp_server);
//! ```
//!
//! With `repo_has_grasp_server = false` and no existing root proposal,
//! `use_pr_kind = false` and the push generates patch events instead.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`) + one vanilla git server
//!    (`"git"`) — **no GRASP server**.
//! 2. Publisher manually runs `ngit init --relay <relay_url> --clone <git_url>
//!    -d --identifier ... --name ...` to publish a kind-30617 announcement
//!    whose `clone` tag contains only the vanilla git server URL (no GRASP
//!    URL). Pushes via the nostr:// remote to graduate the announcement and
//!    seed the bare git repo.
//! 3. Contributor clones from the nostr:// URL and creates a fresh account.
//! 4. Contributor checks out `pr/feature`, makes two commits (`t1.md`,
//!    `t2.md`).
//! 5. Contributor runs `git push -u origin pr/feature`.
//! 6. [`capture_snapshot`] queries the relay for events.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Zero KIND_PULL_REQUEST events — the GRASP-default PR path must not fire
//!    when the announcement has no GRASP server.
//! 2. At least one Kind::GitPatch event — the push must fall through to the
//!    traditional patch-kind path.
//! 3. The root patch's `branch-name` tag equals `"feature"` — confirms the
//!    `pr/` prefix is stripped correctly even on the patch-kind path.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, KIND_PULL_REQUEST, tag_value};
use tokio::sync::OnceCell;

/// Identifier for this test repo — distinct from every other test repo to
/// prevent cross-test relay pollution on the shared vanilla relay surface.
const IDENTIFIER: &str = "git-push-pr-patch-kind-when-no-grasp";

/// Feature branch name; pushed as `pr/feature`. The `branch-name` tag on
/// the root patch event should carry `"feature"` (with the `pr/` prefix
/// stripped by `make_branch_name_tag_from_check_out_branch`).
const BRANCH: &str = "feature";

/// Path component appended to the vanilla server's base URL. The server
/// routes all requests to its single bare repo regardless of path prefix,
/// so any `.git`-suffixed name works; we pick a descriptive one.
const GIT_REPO_PATH: &str = "/repo.git";

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// All observable side-effects of one `git push -u origin pr/feature`
/// against a no-GRASP repo, captured once by [`capture_snapshot`] and
/// shared read-only across the three `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// Total KIND_PULL_REQUEST events authored by the contributor on the
    /// relay. Must equal 0 (case 1) — the GRASP-default PR path must not
    /// fire when `repo_has_grasp_server = false`.
    pr_count: usize,

    /// Total Kind::GitPatch events authored by the contributor on the
    /// relay. Must be >= 1 (case 2) — the patch-kind fallback path must
    /// fire.
    patch_count: usize,

    /// The root patch event: the `Kind::GitPatch` carrying `["t", "root"]`,
    /// or the first patch event if no root-marker is present. This is the
    /// event that carries the `branch-name` tag (case 3).
    cover_letter_event_or_first_patch: Event,

    /// Value of the `branch-name` tag on `cover_letter_event_or_first_patch`.
    /// Must equal `"feature"` (case 3).
    patch_event_branch_name_tag: Option<String>,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: initialise [`SNAPSHOT`] exactly once per binary, hand
/// every test case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(capture_snapshot().await.expect(
                "git_push_pr::patch_kind_when_no_grasp fixture: \
                         capture_snapshot failed",
            ))
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- 1. Harness: vanilla relay + vanilla git server, NO GRASP ----------
    //
    // `VanillaGitServer` requires a multi-thread runtime because the accept
    // loop is a spawned task; git pushes from the test thread need a worker
    // thread available. Every `#[rstest]` in this file therefore carries
    // `#[tokio::test(flavor = "multi_thread")]`.
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("git")
    .build()
    .await?;

    let relay_url = harness.relay("default").url().to_string();
    // Append a `.git`-suffixed path so ngit's URL checks treat it as a
    // direct git URL rather than trying to reformat it as a GRASP URL.
    let git_server_url = format!(
        "{}{}",
        harness.vanilla_git_server("git").url(),
        GIT_REPO_PATH,
    );

    // --- 2. Publisher: manual setup (publish_repo requires a GRASP) --------
    //
    // We use ngit init --relay + --clone (without --grasp-server) so the
    // kind-30617 announcement carries only the vanilla git server URL in its
    // `clone` tag. That makes `repo_ref.grasp_servers()` return an empty list
    // and `repo_has_grasp_server = false` in push.rs:648.
    let publisher = harness.fresh_repo()?;

    let init_out = publisher
        .ngit([
            "account",
            "create",
            "--local",
            "--name",
            "patch-kind-no-grasp maintainer",
        ])
        .output()
        .await
        .context("failed to spawn ngit account create for publisher")?;
    if !init_out.status.success() {
        bail!(
            "ngit account create (publisher) exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    std::fs::write(publisher.dir().join("README.md"), "hello\n")
        .context("failed to write seed README.md")?;
    publisher
        .git_ok(["add", "README.md"], "git add README.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "initial", "--no-gpg-sign"],
            "git commit initial",
        )
        .await?;

    // Run ngit init with --relay and --clone but NO --grasp-server.
    // `has_both_relays_and_clone_url` (init.rs:265) suppresses the
    // "missing grasp server" prompt so this runs non-interactively via -d.
    let init_out = publisher
        .ngit([
            "init",
            "--relay",
            &relay_url,
            "--clone",
            &git_server_url,
            "-d",
            "--identifier",
            IDENTIFIER,
            "--name",
            "patch-kind-no-grasp maintainer",
        ])
        .output()
        .await
        .context("failed to spawn ngit init")?;
    if !init_out.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    let init_stdout = String::from_utf8_lossy(&init_out.stdout);
    let clone_url = extract_clone_url(&init_stdout).with_context(|| {
        format!(
            "no `clone url:` line in ngit init stdout — has the print format changed?\n\
             full stdout was:\n{init_stdout}"
        )
    })?;

    // Push main via the nostr:// remote to graduate the announcement (move it
    // out of purgatory) and push the seed commit to the vanilla git server.
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("git push -u origin main (graduation) failed")?;

    // --- 3. Clone as a fresh contributor ------------------------------------
    let contributor = harness
        .clone_url(&clone_url)
        .await
        .context("git clone from nostr:// URL failed")?;

    let account_out = contributor
        .ngit([
            "account",
            "create",
            "--local",
            "--name",
            "patch-kind-no-grasp contributor",
        ])
        .output()
        .await
        .context("failed to spawn ngit account create for contributor")?;
    if !account_out.status.success() {
        bail!(
            "ngit account create (contributor) exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            account_out.status,
            String::from_utf8_lossy(&account_out.stdout),
            String::from_utf8_lossy(&account_out.stderr),
        );
    }

    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after contributor account create")?;
    let contributor_keys =
        Keys::parse(&contributor_nsec).context("contributor nostr.nsec is not a valid key")?;
    let contributor_pubkey = contributor_keys.public_key();

    // --- 4. Contributor: checkout pr/feature, make two commits -------------
    //
    // The `pr/` prefix triggers the proposal code path in push.rs. With no
    // GRASP server in the announcement, `repo_has_grasp_server = false`
    // and the condition `root_proposal.is_none() && repo_has_grasp_server`
    // evaluates to `false` — the push falls through to Kind::GitPatch.
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

    // --- 5. Push pr/feature -------------------------------------------------
    contributor
        .nostr_push(["-u", "origin", &format!("pr/{BRANCH}")])
        .await
        .context("nostr_push -u origin pr/feature failed")?;

    // --- 6. Capture events from the vanilla relay ---------------------------
    let pr_events = harness
        .relay("default")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count = pr_events.len();

    let patch_events: Vec<Event> = harness
        .relay("default")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(Kind::GitPatch),
        )
        .await?;
    let patch_count = patch_events.len();

    // The root patch is the Kind::GitPatch carrying `["t", "root"]` — the
    // event that `make_cover_letter_patch` / `make_patch` marks as the
    // series root in git_events.rs. Fall back to the first patch if the push
    // produced only a single event without an explicit root marker.
    let cover_letter_event_or_first_patch = patch_events
        .iter()
        .find(|e| {
            e.tags.iter().any(|t| {
                let s = t.as_slice();
                s.first().map(String::as_str) == Some("t")
                    && s.get(1).map(String::as_str) == Some("root")
            })
        })
        .or_else(|| patch_events.first())
        .cloned()
        .context(
            "no Kind::GitPatch events found on relay after contributor push — \
             did the push produce no patch events at all?",
        )?;

    let patch_event_branch_name_tag = tag_value(&cover_letter_event_or_first_patch, "branch-name");

    Ok(Snapshot {
        pr_count,
        patch_count,
        cover_letter_event_or_first_patch,
        patch_event_branch_name_tag,
    })
}

/// Extract the `nostr://…` clone URL from `ngit init` stdout.
/// Mirrors the private `extract_clone_url` in `test_harness::scenarios`.
fn extract_clone_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("clone url:") {
            let rest = line[idx + "clone url:".len()..].trim();
            if rest.starts_with("nostr://") {
                return Some(rest.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Case 1: Zero KIND_PULL_REQUEST events on the relay.
///
/// The `new_pr` test in this module guards the opposite direction —
/// that a GRASP-backed repo _does_ produce a PR kind event. This case
/// guards the regression where removing or short-circuiting the
/// `repo_has_grasp_server` condition causes patch pushes to
/// accidentally produce PR events even when no GRASP is present.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn zero_pr_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 0,
        "expected zero KIND_PULL_REQUEST events on the relay authored by contributor; \
         got {} — the GRASP-default PR path (push.rs:648-652) must not fire when the \
         repo announcement has no GRASP server",
        s.pr_count,
    );
    Ok(())
}

/// Case 2: At least one Kind::GitPatch event on the relay.
///
/// Without a GRASP server in the announcement, the push must fall through
/// to the traditional patch-kind path. An empty patch count means the push
/// produced nothing — either the code path was removed or the condition
/// logic inverted.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn at_least_one_patch_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert!(
        s.patch_count >= 1,
        "expected at least one Kind::GitPatch event on the relay authored by contributor; \
         got {} — the patch-kind fallback path did not fire for a no-GRASP repo",
        s.patch_count,
    );
    Ok(())
}

/// Case 3: The root patch event's `branch-name` tag equals `"feature"`.
///
/// `make_branch_name_tag_from_check_out_branch` (git_events.rs:657) strips
/// the `pr/` prefix before writing the tag. An incorrect value (e.g.
/// `"pr/feature"`) would break `ngit list` and `ngit pr checkout` on
/// patch-kind proposals, and is especially easy to introduce on the
/// patch-kind code path if the tag-generation is only exercised via the
/// PR-kind path in other tests.
///
/// Also confirms the event kind is `Kind::GitPatch` — not
/// `KIND_PULL_REQUEST` sneaking in as the first event.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn patch_event_branch_name_tag_is_feature(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.cover_letter_event_or_first_patch.kind,
        Kind::GitPatch,
        "root patch event should be Kind::GitPatch; got {:?}",
        s.cover_letter_event_or_first_patch.kind,
    );
    assert_eq!(
        s.patch_event_branch_name_tag.as_deref(),
        Some(BRANCH),
        "root Kind::GitPatch event branch-name tag should be {:?} (pr/ prefix stripped); \
         got {:?}",
        BRANCH,
        s.patch_event_branch_name_tag,
    );
    Ok(())
}
