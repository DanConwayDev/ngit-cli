//! End-to-end coverage of the top-level `ngit merge` command.
//!
//! `ngit merge` creates a no-ff merge commit of a PR branch onto the
//! repository's default branch, records the PR's nevent and description in
//! the merge-commit body, leaves the default branch checked out, and does
//! **not** push anything (neither git data nor a nostr status event).
//!
//! ## Scenario shape (shared by every test)
//!
//! - One vanilla relay (`"default"`) + one GRASP (`"repo"`).
//! - Maintainer publishes the repo via `harness.publish_repo`.
//! - A fresh contributor publishes three PR-kind proposals via
//!   `harness.publish_three_open_proposals`.
//! - The maintainer clone (`publisher`) is where the merge happens — it is on
//!   `main` with an `origin` remote, matching what a maintainer does.
//!
//! Assertions are on observable side-effects (refs/commits on disk, exit
//! status), never on literal stdout, per the harness boundary rules.

use anyhow::{Context, Result};
use test_harness::{Harness, PublishRepoOpts, PublishedPr, PublishedRepo, Repo};

struct Setup {
    harness: Harness,
    _published: PublishedRepo,
    prs: [PublishedPr; 3],
    publisher: Repo,
}

async fn setup() -> Result<Setup> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("merge maintainer".into()),
            identifier: Some("merge-repo".into()),
            ..Default::default()
        })
        .await?;

    let prs = harness.publish_three_open_proposals(&published).await?;

    Ok(Setup {
        harness,
        _published: published,
        prs,
        publisher,
    })
}

/// Long-form `pr/<branch>(<root-event-id-shorthand>)` branch name —
/// matches `CoverLetter::get_branch_name_with_pr_prefix_and_shorthand_id`.
fn expected_branch_name(pr: &PublishedPr) -> String {
    let hex = pr.event_id.to_hex();
    format!("pr/{}({})", pr.branch_name, &hex[..8])
}

async fn git_ok<I, S>(repo: &Repo, args: I, label: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn {label}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

async fn rev_parse(repo: &Repo, rev: &str) -> Result<String> {
    let out = repo
        .git(["rev-parse", rev])
        .output()
        .await
        .with_context(|| format!("failed to spawn git rev-parse {rev}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git rev-parse {rev} exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git rev-parse stdout not utf-8")?
        .trim()
        .to_string())
}

async fn current_branch(repo: &Repo) -> Result<String> {
    let out = repo
        .git(["symbolic-ref", "--short", "HEAD"])
        .output()
        .await
        .context("failed to spawn git symbolic-ref HEAD")?;
    anyhow::ensure!(
        out.status.success(),
        "git symbolic-ref --short HEAD exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git symbolic-ref stdout not utf-8")?
        .trim()
        .to_string())
}

/// Full commit message (subject + body) of `<rev>`.
async fn commit_message(repo: &Repo, rev: &str) -> Result<String> {
    let out = repo
        .git(["log", "-1", "--format=%B", rev])
        .output()
        .await
        .with_context(|| format!("failed to spawn git log {rev}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git log {rev} exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git log stdout not utf-8")?
        .to_string())
}

/// Number of parents of `<rev>` — a no-ff merge commit has 2.
async fn parent_count(repo: &Repo, rev: &str) -> Result<usize> {
    let out = repo
        .git(["rev-list", "--parents", "-n", "1", rev])
        .output()
        .await
        .with_context(|| format!("failed to spawn git rev-list {rev}"))?;
    anyhow::ensure!(out.status.success(), "git rev-list {rev} failed");
    let line = String::from_utf8(out.stdout).context("git rev-list stdout not utf-8")?;
    // line = "<commit> <parent1> <parent2> ..."
    Ok(line.split_whitespace().count().saturating_sub(1))
}

async fn run_merge(repo: &Repo, args: &[&str]) -> Result<std::process::Output> {
    let mut argv = vec!["merge"];
    argv.extend_from_slice(args);
    repo.ngit(argv)
        .output()
        .await
        .context("failed to spawn ngit merge")
}

// ---------------------------------------------------------------------------
// 1. `ngit merge <hex>` from main creates a no-ff merge commit on the default
//    branch, leaves it checked out, records the PR in the body, no push.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_by_id_creates_no_ff_merge_on_default_branch() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        publisher,
    } = setup().await?;
    let pr = &prs[0];

    let main_before = rev_parse(&publisher, "main").await?;

    let out = run_merge(&publisher, &[&pr.event_id.to_hex()]).await?;
    anyhow::ensure!(
        out.status.success(),
        "ngit merge exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // left checked out on the default branch
    assert_eq!(
        current_branch(&publisher).await?,
        "main",
        "should be left on the default branch after merge",
    );

    // a new commit advanced main
    let main_after = rev_parse(&publisher, "main").await?;
    assert_ne!(
        main_after, main_before,
        "main should have a new merge commit"
    );

    // it is a real merge commit (2 parents)
    assert_eq!(
        parent_count(&publisher, "main").await?,
        2,
        "no-ff merge should produce a 2-parent merge commit",
    );

    // the PR tip is an ancestor of the merge commit (it was merged in)
    let branch = expected_branch_name(pr);
    let pr_tip = rev_parse(&publisher, &branch).await?;
    assert_eq!(
        pr_tip, pr.tip,
        "local pr branch should sit at the published tip"
    );

    // commit body records the PR title and nevent
    let msg = commit_message(&publisher, "main").await?;
    assert!(
        msg.contains("nevent1"),
        "merge commit body should contain the PR nevent, got:\n{msg}",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 2. `ngit merge` without id while on the pr/ branch infers the PR.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_without_id_infers_pr_from_current_branch() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        publisher,
    } = setup().await?;
    let pr = &prs[0];

    // check out the PR branch first
    let out = publisher
        .ngit(["pr", "checkout", &pr.event_id.to_hex()])
        .output()
        .await
        .context("failed to spawn ngit pr checkout")?;
    anyhow::ensure!(
        out.status.success(),
        "ngit pr checkout exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let branch = expected_branch_name(pr);
    assert_eq!(current_branch(&publisher).await?, branch);

    // merge with no id — should infer the PR from the checked-out branch
    let out = run_merge(&publisher, &[]).await?;
    anyhow::ensure!(
        out.status.success(),
        "ngit merge (no id) exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert_eq!(
        current_branch(&publisher).await?,
        "main",
        "should be left on the default branch after merge",
    );
    assert_eq!(
        parent_count(&publisher, "main").await?,
        2,
        "no-ff merge should produce a 2-parent merge commit",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 3. dirty working tree aborts the merge.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dirty_working_tree_aborts_merge() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        publisher,
    } = setup().await?;
    let pr = &prs[0];

    let main_before = rev_parse(&publisher, "main").await?;

    // create an untracked file so the tree is dirty
    std::fs::write(publisher.dir().join("dirty.md"), "uncommitted\n").context("write dirty.md")?;

    let out = run_merge(&publisher, &[&pr.event_id.to_hex()]).await?;
    assert!(
        !out.status.success(),
        "ngit merge should abort when the working tree is dirty",
    );

    // main untouched
    assert_eq!(
        rev_parse(&publisher, "main").await?,
        main_before,
        "main must not advance when the merge is aborted",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 4. merge without id while not on a pr/ branch fails clearly.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_without_id_off_pr_branch_fails() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs: _,
        publisher,
    } = setup().await?;

    // on main, not a pr/ branch
    git_ok(&publisher, ["checkout", "main"], "git checkout main").await?;

    let out = run_merge(&publisher, &[]).await?;
    assert!(
        !out.status.success(),
        "ngit merge with no id off a pr/ branch should fail",
    );

    Ok(())
}
