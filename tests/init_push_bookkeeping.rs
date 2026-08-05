//! `ngit init`'s in-process push bookkeeping and its seam with the
//! remote helper.
//!
//! `ngit init` pushes the initial branch in-process through the state
//! transaction instead of shelling out to `git push`, so git's own
//! post-push side effects never run. `push_bookkeeping` replicates the
//! two that matter — remote-tracking-ref maintenance (with git's
//! `update by push` reflog message) and `git push -u`'s upstream
//! config. Subsequent pushes go through `git-remote-nostr`, where git
//! itself performs the tracking update after the helper reports `ok`
//! (commit 73d0e57 removed the helper's redundant writes). These tests
//! pin both writers and the boundary between them:
//!
//! - [`fresh_init_replicates_git_push_bookkeeping`] — after a fresh `ngit
//!   init`, the tracking ref, its reflog message, and the upstream config all
//!   look exactly as if `git push -u origin main` had run.
//! - [`helper_push_after_init_advances_and_prunes_tracking_ref`] — the seam:
//!   pushes through the remote helper against the init-created remote advance
//!   the tracking ref (git's post-`ok` update), and an accepted `--delete`
//!   removes it.
//! - [`narrowed_fetch_refspec_writes_no_out_of_refspec_tracking_ref`] —
//!   regression cover for 73d0e57's behaviour correction: with
//!   `remote.origin.fetch` narrowed to `main`, pushing another branch writes
//!   *no* tracking ref (git maps destinations through the fetch refspecs; the
//!   helper must not write what git would not), while the push itself still
//!   succeeds server-side.
//!
//! Assertions target refs, config, reflogs, and relay events only —
//! never ngit's stdout. Every push completes before its assertions run
//! (init's subprocess exit and `Repo::nostr_push`'s success are the
//! barriers), so nothing here polls or sleeps.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{ArrangedInitStateA, Harness, KIND_REPO_STATE, Repo, tag_value};

/// The branch `ngit init` pushes: `Repo::init` runs `git init -b main`
/// and the State A arrange commits on it.
const BRANCH: &str = "main";

/// Reflog message git writes when `git push` updates a remote-tracking
/// ref; `push_bookkeeping::record_accepted_push_refspecs` must match it.
const PUSH_REFLOG_MESSAGE: &str = "update by push";

/// Build the one-relay one-grasp harness every test here uses.
async fn build_harness() -> Result<Harness> {
    Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await
}

/// Arrange a State A repo (account + seed commits on `main`) and run a
/// fresh `ngit init --name <name> --identifier <identifier>
/// --grasp-server <url>`, which takes the `PushInitialBranch` action:
/// an in-process push of `main` plus the bookkeeping under test. The
/// subprocess does not exit until that push completes, so callers
/// assert immediately.
async fn arrange_and_init(
    harness: &Harness,
    name: &str,
    identifier: &str,
) -> Result<(Repo, ArrangedInitStateA)> {
    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let grasp_url = harness.grasp("repo").url().to_string();
    let out = repo
        .ngit([
            "init",
            "--name",
            name,
            "--identifier",
            identifier,
            "--grasp-server",
            &grasp_url,
        ])
        .output()
        .await
        .context("failed to spawn ngit init")?;
    if !out.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok((repo, state))
}

/// Most recent reflog message of `refname`, read via git plumbing.
async fn last_reflog_message(repo: &Repo, refname: &str) -> Result<String> {
    let out = repo
        .git(["reflog", "--format=%gs", "-n", "1", refname])
        .output()
        .await
        .with_context(|| format!("failed to spawn git reflog for {refname}"))?;
    if !out.status.success() {
        bail!(
            "git reflog {refname} exited non-zero ({:?}) — does the ref have a reflog?\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(String::from_utf8(out.stdout)
        .context("git reflog returned non-utf8")?
        .trim()
        .to_string())
}

/// Fresh `ngit init` leaves the repo exactly as `git push -u origin
/// main` would have: tracking ref at the local tip, git's push reflog
/// message, and `branch.main.{remote,merge}` set. This is the
/// integration-level contract of `push_bookkeeping`'s two entry points,
/// previously covered only by unit tests.
#[tokio::test]
async fn fresh_init_replicates_git_push_bookkeeping() -> Result<()> {
    let harness = build_harness().await?;
    let (repo, _state) = arrange_and_init(&harness, "init bookkeeping", "init-bookkeeping").await?;

    let tracking_ref = format!("refs/remotes/origin/{BRANCH}");
    let snap = repo.snapshot()?;
    let local = snap
        .refs
        .get(&format!("refs/heads/{BRANCH}"))
        .with_context(|| format!("refs/heads/{BRANCH} missing after ngit init"))?;
    let tracking = snap.refs.get(&tracking_ref).with_context(|| {
        format!(
            "{tracking_ref} missing after ngit init — the in-process push \
             skipped git's tracking-ref bookkeeping"
        )
    })?;
    assert_eq!(
        tracking, local,
        "{tracking_ref} ({tracking}) does not match the pushed local tip ({local})",
    );

    let reflog_message = last_reflog_message(&repo, &tracking_ref).await?;
    assert_eq!(
        reflog_message, PUSH_REFLOG_MESSAGE,
        "last reflog entry on {tracking_ref} is {reflog_message:?}; git \
         writes {PUSH_REFLOG_MESSAGE:?} after an accepted push and \
         push_bookkeeping must match it",
    );

    assert_eq!(
        repo.config(&format!("branch.{BRANCH}.remote"))
            .await?
            .as_deref(),
        Some("origin"),
        "branch.{BRANCH}.remote not set to origin — init skipped `git push -u`'s upstream setup",
    );
    assert_eq!(
        repo.config(&format!("branch.{BRANCH}.merge"))
            .await?
            .as_deref(),
        Some(format!("refs/heads/{BRANCH}").as_str()),
        "branch.{BRANCH}.merge not set — init skipped `git push -u`'s upstream setup",
    );
    Ok(())
}

/// The seam between init's in-process bookkeeping and the remote
/// helper: after init, a `git push` through `git-remote-nostr` must
/// advance the tracking ref (git's own post-`ok` update — the helper
/// writes nothing since 73d0e57), and an accepted branch deletion must
/// prune its tracking ref. If the helper stopped reporting `ok` lines
/// git can map, both halves of this test fail.
#[tokio::test]
async fn helper_push_after_init_advances_and_prunes_tracking_ref() -> Result<()> {
    let harness = build_harness().await?;
    let (repo, _state) = arrange_and_init(&harness, "init seam", "init-seam").await?;

    // Advance main past the tip init pushed, then push through the
    // remote helper.
    std::fs::write(repo.dir().join("feature.txt"), "advance main\n")
        .context("failed to write feature.txt")?;
    repo.git_ok(["add", "feature.txt"], "git add feature.txt")
        .await?;
    repo.git_ok(
        ["commit", "-m", "advance main", "--no-gpg-sign"],
        "git commit on main",
    )
    .await?;
    let new_tip = repo.rev_parse(&format!("refs/heads/{BRANCH}")).await?;

    repo.nostr_push(["origin", BRANCH])
        .await
        .context("git push origin main after init")?;

    let main_tracking_ref = format!("refs/remotes/origin/{BRANCH}");
    let snap = repo.snapshot()?;
    assert_eq!(
        snap.refs.get(&main_tracking_ref),
        Some(&new_tip),
        "{main_tracking_ref} did not advance to the new tip after a push \
         through the remote helper",
    );
    let reflog_message = last_reflog_message(&repo, &main_tracking_ref).await?;
    assert_eq!(
        reflog_message, PUSH_REFLOG_MESSAGE,
        "git's own post-push tracking update should log {PUSH_REFLOG_MESSAGE:?} \
         on {main_tracking_ref}; got {reflog_message:?}",
    );

    // Push a topic branch (tracking ref appears), then delete it on the
    // remote (tracking ref must be pruned). HEAD returns to main before
    // each push so the published default branch never moves.
    let topic = "topic";
    let topic_tracking_ref = format!("refs/remotes/origin/{topic}");
    repo.git_ok(["checkout", "-b", topic], "git checkout -b topic")
        .await?;
    std::fs::write(repo.dir().join("topic.txt"), "topic work\n")
        .context("failed to write topic.txt")?;
    repo.git_ok(["add", "topic.txt"], "git add topic.txt")
        .await?;
    repo.git_ok(
        ["commit", "-m", "topic work", "--no-gpg-sign"],
        "git commit on topic",
    )
    .await?;
    let topic_tip = repo.rev_parse(&format!("refs/heads/{topic}")).await?;
    repo.git_ok(["checkout", BRANCH], "git checkout main")
        .await?;

    repo.nostr_push(["origin", topic])
        .await
        .context("git push origin topic")?;
    assert_eq!(
        repo.snapshot()?.refs.get(&topic_tracking_ref),
        Some(&topic_tip),
        "{topic_tracking_ref} missing or stale after pushing the topic branch",
    );

    repo.nostr_push(["origin", "--delete", topic])
        .await
        .context("git push origin --delete topic")?;
    let snap = repo.snapshot()?;
    assert!(
        !snap.refs.contains_key(&topic_tracking_ref),
        "{topic_tracking_ref} survived an accepted `git push origin --delete \
         {topic}` — git should have pruned it after the helper's `ok`",
    );
    assert_eq!(
        snap.refs.get(&main_tracking_ref),
        Some(&new_tip),
        "deleting {topic} on the remote disturbed {main_tracking_ref}",
    );
    Ok(())
}

/// Regression cover for 73d0e57's behaviour correction: git only writes
/// tracking refs for destinations covered by `remote.<name>.fetch`.
/// With the refspec narrowed to `main`, pushing another branch must
/// leave `refs/remotes/origin/<other>` absent — the helper writing it
/// anyway (the pre-73d0e57 behaviour) is exactly what this catches.
/// The push itself still succeeds: the branch lands in the kind-30618
/// state event on the grasp.
#[tokio::test]
async fn narrowed_fetch_refspec_writes_no_out_of_refspec_tracking_ref() -> Result<()> {
    let identifier = "init-narrow-refspec";
    let harness = build_harness().await?;
    let (repo, state) = arrange_and_init(&harness, "init narrow refspec", identifier).await?;

    let main_tracking_ref = format!("refs/remotes/origin/{BRANCH}");
    let init_main_tracking = repo
        .snapshot()?
        .refs
        .get(&main_tracking_ref)
        .with_context(|| format!("{main_tracking_ref} missing after ngit init"))?
        .clone();

    // Narrow the fetch refspec from the default `+refs/heads/*` to main
    // only; `outside` pushes are then out of refspec for tracking.
    repo.git_ok(
        [
            "config",
            "--local",
            "remote.origin.fetch",
            &format!("+refs/heads/{BRANCH}:refs/remotes/origin/{BRANCH}"),
        ],
        "git config remote.origin.fetch (narrowed)",
    )
    .await?;

    let outside = "outside";
    repo.git_ok(["checkout", "-b", outside], "git checkout -b outside")
        .await?;
    std::fs::write(repo.dir().join("outside.txt"), "out of refspec\n")
        .context("failed to write outside.txt")?;
    repo.git_ok(["add", "outside.txt"], "git add outside.txt")
        .await?;
    repo.git_ok(
        ["commit", "-m", "outside work", "--no-gpg-sign"],
        "git commit on outside",
    )
    .await?;
    let outside_tip = repo.rev_parse(&format!("refs/heads/{outside}")).await?;
    repo.git_ok(["checkout", BRANCH], "git checkout main")
        .await?;

    repo.nostr_push(["origin", outside])
        .await
        .context("git push origin outside")?;

    let snap = repo.snapshot()?;
    let outside_tracking_ref = format!("refs/remotes/origin/{outside}");
    assert!(
        !snap.refs.contains_key(&outside_tracking_ref),
        "{outside_tracking_ref} exists after pushing a branch outside the \
         narrowed remote.origin.fetch — someone reintroduced helper-side \
         tracking-ref writes that diverge from git (see commit 73d0e57)",
    );
    assert_eq!(
        snap.refs.get(&main_tracking_ref),
        Some(&init_main_tracking),
        "pushing {outside} disturbed {main_tracking_ref}",
    );

    // The push itself was accepted: the state event the helper emitted
    // covers the branch at its tip. `Repo::nostr_push` returning success
    // guarantees the event is already queryable on the grasp.
    let state_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(state.keys.public_key())
                .kind(KIND_REPO_STATE),
        )
        .await?;
    let state_event = state_events
        .iter()
        .find(|e| tag_value(e, "d").as_deref() == Some(identifier))
        .with_context(|| {
            format!("no kind-30618 state event with `d` = {identifier:?} on the grasp")
        })?;
    assert_eq!(
        tag_value(state_event, &format!("refs/heads/{outside}")).as_deref(),
        Some(outside_tip.as_str()),
        "state event does not list refs/heads/{outside} at the pushed tip — \
         the out-of-refspec push did not reach the server",
    );
    Ok(())
}
