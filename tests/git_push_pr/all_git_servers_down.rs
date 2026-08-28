//! Failure-path coverage: proposal refs must not be reported pushed when
//! every git server is unreachable.
//!
//! ## What this pins
//!
//! In `run_push` (src/bin/ngit/git_remote_helper/push.rs), proposal
//! events sit in `other_events`, which is published only when the state
//! transaction reports that a git server accepted the pushed data
//! (`Ok(())` or `Err(StateNotAcceptedByAnyRelay)`). Historically the
//! helper printed `ok <ref>` for proposal refspecs and wrote their
//! remote-tracking refs *before* the transaction ran, so a push whose
//! git-server phase failed entirely (`NoEligibleGitServers` /
//! `AllGitServerPushesFailed`) told git the proposal ref was pushed even
//! though its events never reached any relay — and git, on seeing `ok`,
//! recorded `refs/remotes/<remote>/pr/<branch>` too.
//!
//! The helper now reports proposal refspecs `ok` only in the arms where
//! their events are actually published, and reports `error <ref>
//! <reason>` in the two total-failure arms, matching the state refspecs.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`) + one vanilla git server
//!    (`"git"`) — no GRASP, so `pr/` pushes take the patch-kind path and
//!    proposal event creation succeeds without needing a live git server (a
//!    GRASP repo would fail earlier, while creating the PR event).
//! 2. Publisher runs `ngit init --additional-relay <relay> --additional-clone
//!    <git_url>` and pushes `main` so the announcement, state event and
//!    tracking refs exist.
//! 3. Publisher advances `main` and creates a `pr/feature` branch with one
//!    commit.
//! 4. The only git server is taken offline (drop kills the listener).
//! 5. Publisher runs `git push origin main pr/feature` — a mixed batch,
//!    required to reach the state transaction's total-failure arms (a pure
//!    `pr/` push with no state refspec never runs the transaction).
//! 6. [`capture_snapshot`] records the push exit status, relay events and local
//!    refs.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. The push exits non-zero — git reports the failure to the user.
//! 2. Zero proposal events (GitPatch and KIND_PULL_REQUEST) authored by the
//!    publisher reached the relay.
//! 3. No `refs/remotes/origin/pr/feature` tracking ref exists — neither the
//!    helper nor git (which only acts on `ok`) recorded one.
//! 4. `refs/remotes/origin/main` still points at the previously pushed tip, not
//!    the unpushed new commit.
//! 5. No kind-30618 state event on the relay references the unpushed tips.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, KIND_PULL_REQUEST};
use tokio::sync::OnceCell;

/// Identifier for this test repo — distinct from every other test repo to
/// prevent cross-test relay pollution on the shared vanilla relay surface.
const IDENTIFIER: &str = "git-push-pr-all-git-servers-down";

/// Feature branch name; pushed as `pr/feature`.
const BRANCH: &str = "feature";

/// Path component appended to the vanilla server's base URL so ngit's URL
/// checks treat it as a direct git URL (see `patch_kind_when_no_grasp.rs`).
const GIT_REPO_PATH: &str = "/repo.git";

/// Kind-30618 repository state event (NIP-34).
const KIND_REPO_STATE: u16 = 30618;

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// All observable side-effects of one failed `git push origin main
/// pr/feature`, captured once by [`capture_snapshot`] and shared across
/// the `#[rstest]` cases via [`SNAPSHOT`].
struct Snapshot {
    /// Whether the push reported success. Must be `false` (case 1).
    push_succeeded: bool,

    /// Total Kind::GitPatch events authored by the publisher on the
    /// relay. Must equal 0 (case 2).
    patch_count: usize,

    /// Total KIND_PULL_REQUEST events authored by the publisher on the
    /// relay. Must equal 0 (case 2).
    pr_kind_count: usize,

    /// Publisher's full ref map after the failed push (cases 3 and 4).
    refs: BTreeMap<String, String>,

    /// `main` tip at the time of the last *successful* push. The
    /// `refs/remotes/origin/main` tracking ref must still equal this
    /// (case 4).
    pushed_main_tip: String,

    /// `main` tip after the local-only "advance main" commit — never
    /// accepted by any server (cases 4 and 5).
    unpushed_main_tip: String,

    /// `pr/feature` tip — never accepted by any server (case 5).
    pr_tip: String,

    /// Number of kind-30618 state events by the publisher on the relay
    /// whose tags reference either unpushed tip. Must equal 0 (case 5).
    state_events_referencing_unpushed_tips: usize,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

/// rstest fixture: initialise [`SNAPSHOT`] exactly once per binary, hand
/// every test case a cheap `Arc` clone.
#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("git_push_pr::all_git_servers_down fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- 1. Harness: vanilla relay + vanilla git server, NO GRASP ----------
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("git")
    .build()
    .await?;

    let relay_url = harness.relay("default").url().to_string();
    let git_server_url = format!(
        "{}{}",
        harness.vanilla_git_server("git").url(),
        GIT_REPO_PATH,
    );

    // --- 2. Publisher: account, seed commit, init, first push --------------
    let publisher = harness.fresh_repo()?;

    let account_out = publisher
        .ngit([
            "account",
            "create",
            "--local",
            "--name",
            "all-servers-down maintainer",
        ])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    if !account_out.status.success() {
        bail!(
            "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            account_out.status,
            String::from_utf8_lossy(&account_out.stdout),
            String::from_utf8_lossy(&account_out.stderr),
        );
    }

    let publisher_nsec = publisher
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after account create")?;
    let publisher_keys =
        Keys::parse(&publisher_nsec).context("publisher nostr.nsec is not a valid key")?;
    let publisher_pubkey = publisher_keys.public_key();

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

    let init_out = publisher
        .ngit([
            "init",
            "--additional-relay",
            &relay_url,
            "--additional-clone",
            &git_server_url,
            "-d",
            "--identifier",
            IDENTIFIER,
            "--name",
            "all-servers-down maintainer",
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

    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("git push -u origin main (graduation) failed")?;
    let pushed_main_tip = publisher.rev_parse("HEAD").await?;

    // --- 3. Advance main and create pr/feature, both locally only ----------
    std::fs::write(publisher.dir().join("t-main.md"), "content\n")
        .context("failed to write t-main.md")?;
    publisher
        .git_ok(["add", "t-main.md"], "git add t-main.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main", "--no-gpg-sign"],
            "git commit advance main",
        )
        .await?;
    let unpushed_main_tip = publisher.rev_parse("HEAD").await?;

    publisher
        .git_ok(
            ["checkout", "-b", &format!("pr/{BRANCH}")],
            &format!("git checkout -b pr/{BRANCH}"),
        )
        .await?;
    std::fs::write(publisher.dir().join("t1.md"), "some content\n")
        .context("failed to write t1.md")?;
    publisher.git_ok(["add", "t1.md"], "git add t1.md").await?;
    publisher
        .git_ok(
            ["commit", "-m", "add t1.md", "--no-gpg-sign"],
            "git commit t1.md",
        )
        .await?;
    let pr_tip = publisher.rev_parse("HEAD").await?;

    // --- 4. Take the only git server offline --------------------------------
    let git_server = harness
        .take_vanilla_git_server("git")
        .context("vanilla git server was never registered or already taken")?;
    let dead_addr = git_server.url().trim_start_matches("http://").to_string();
    drop(git_server);

    // Sanity: the server must actually be unreachable, otherwise the push
    // could succeed and the test would assert nothing about the failure path.
    let probe = tokio::net::TcpStream::connect(&dead_addr).await;
    assert!(
        probe.is_err(),
        "git server should be unreachable after drop, but TCP connect to \
         {dead_addr} succeeded — cannot test all-servers-down path",
    );

    // --- 5. Mixed push: state refspec + proposal refspec ---------------------
    //
    // The state refspec (`main`) is required: without one the helper never
    // runs the state transaction, and the total-failure arms under test are
    // unreachable. Both refs must come back failed.
    let push_out = publisher
        .nostr_push_expecting_failure(["origin", "main", &format!("pr/{BRANCH}")])
        .await
        .context("push with all git servers down")?;
    let push_succeeded = push_out.status.success();

    // --- 6. Capture relay events and local refs ------------------------------
    let patch_count = harness
        .relay("default")
        .events(Filter::new().author(publisher_pubkey).kind(Kind::GitPatch))
        .await?
        .len();
    let pr_kind_count = harness
        .relay("default")
        .events(
            Filter::new()
                .author(publisher_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?
        .len();

    let state_events = harness
        .relay("default")
        .events(
            Filter::new()
                .author(publisher_pubkey)
                .kind(Kind::Custom(KIND_REPO_STATE)),
        )
        .await?;
    let state_events_referencing_unpushed_tips = state_events
        .iter()
        .filter(|e| {
            e.tags.iter().any(|t| {
                t.as_slice()
                    .iter()
                    .any(|v| v == &unpushed_main_tip || v == &pr_tip)
            })
        })
        .count();

    let refs = publisher
        .snapshot()
        .context("capturing publisher snapshot after failed push")?
        .refs;

    Ok(Snapshot {
        push_succeeded,
        patch_count,
        pr_kind_count,
        refs,
        pushed_main_tip,
        unpushed_main_tip,
        pr_tip,
        state_events_referencing_unpushed_tips,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Case 1: the push exits non-zero — git reports failure for the batch
/// instead of pretending the proposal ref was pushed.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn push_reports_failure(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert!(
        !s.push_succeeded,
        "git push with every git server unreachable should exit non-zero",
    );
    Ok(())
}

/// Case 2: zero proposal events reached the relay. The proposal events
/// were generated (patch-kind creation needs no git server) but must not
/// be published when no git server accepted the pushed data.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn no_proposal_events_published(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        (s.patch_count, s.pr_kind_count),
        (0, 0),
        "expected zero GitPatch and zero KIND_PULL_REQUEST events by the publisher \
         after a totally failed push; got {} patches and {} PRs",
        s.patch_count,
        s.pr_kind_count,
    );
    Ok(())
}

/// Case 3: no remote-tracking ref exists for the pr branch. The helper
/// must report `error` (not `ok`) for the proposal refspec, so neither
/// the helper's bookkeeping nor git's own post-`ok` tracking-ref update
/// records `refs/remotes/origin/pr/feature`.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn no_pr_tracking_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tracking_ref = format!("refs/remotes/origin/pr/{BRANCH}");
    assert!(
        !s.refs.contains_key(&tracking_ref),
        "{tracking_ref} exists at {:?} after a totally failed push — the helper \
         reported the proposal refspec pushed without its events reaching any relay",
        s.refs.get(&tracking_ref),
    );
    Ok(())
}

/// Case 4: `refs/remotes/origin/main` still points at the last tip a git
/// server actually accepted, not the unpushed local commit.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn main_tracking_ref_unchanged(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.refs.get("refs/remotes/origin/main"),
        Some(&s.pushed_main_tip),
        "refs/remotes/origin/main should still equal the previously pushed tip \
         {} after a failed push; unpushed tip was {}",
        s.pushed_main_tip,
        s.unpushed_main_tip,
    );
    Ok(())
}

/// Case 5: no kind-30618 state event on the relay references either
/// unpushed tip — the failed transaction must not have broadcast the
/// candidate state.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn no_state_event_references_unpushed_tips(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.state_events_referencing_unpushed_tips, 0,
        "found {} kind-30618 state event(s) referencing an unpushed tip \
         ({} or {}) after a totally failed push",
        s.state_events_referencing_unpushed_tips, s.unpushed_main_tip, s.pr_tip,
    );
    Ok(())
}
