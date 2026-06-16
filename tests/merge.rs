//! End-to-end coverage of the top-level `ngit merge` command.
//!
//! `ngit merge` creates a no-ff merge commit of a PR branch onto the
//! repository's default branch with a `Merge #<id>: <title>` subject, records
//! the PR's nevent and a `PR-Author:` trailer (npub always; display name only
//! when the author's kind-0 metadata is cached), plus (unless
//! `--exclude-description`) the latest cover note or PR description in the
//! merge-commit body, leaves the default branch checked out, and does **not**
//! push anything (neither git data nor a nostr status event).
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
use nostr::nips::nip19::ToBech32;
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedPr, PublishedRepo, Repo};

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

    // commit subject is `Merge #<hex8>: <title>`; body records the nevent (as
    // a bare `nostr:` URI) and the PR description under a header.
    let msg = commit_message(&publisher, "main").await?;
    let shorthand = &pr.event_id.to_hex()[..8];
    let expected_subject = format!("Merge #{shorthand}: proposal");
    assert!(
        msg.lines()
            .next()
            .unwrap_or_default()
            .starts_with(&expected_subject),
        "merge commit subject should start with '{expected_subject}', got:\n{msg}",
    );
    assert!(
        msg.contains("nostr:nevent1"),
        "merge commit body should contain the PR nevent as a nostr: URI, got:\n{msg}",
    );
    assert!(
        msg.contains("PR description:"),
        "merge commit body should contain the PR description header, got:\n{msg}",
    );

    // the PR author is attributed: a `PR-Author:` trailer carrying the
    // author's npub on its own bare `nostr:` URI line. The display name is
    // best-effort (only when kind-0 metadata is in cache) so it is not
    // asserted here; see `author_trailer_carries_author_npub`.
    let author_npub = pr
        .author_pubkey
        .to_bech32()
        .context("failed to bech32-encode PR author pubkey")?;
    assert!(
        msg.contains("PR-Author:"),
        "merge commit body should contain a PR-Author trailer, got:\n{msg}",
    );
    assert!(
        msg.contains(&format!("nostr:{author_npub}")),
        "merge commit body should attribute the author by npub, got:\n{msg}",
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
// 4. --exclude-description omits the PR description footer but keeps the
//    subject and the nevent reference.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exclude_description_omits_body_footer() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        publisher,
    } = setup().await?;
    let pr = &prs[0];

    let out = run_merge(
        &publisher,
        &[&pr.event_id.to_hex(), "--exclude-description"],
    )
    .await?;
    anyhow::ensure!(
        out.status.success(),
        "ngit merge --exclude-description exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let msg = commit_message(&publisher, "main").await?;
    let shorthand = &pr.event_id.to_hex()[..8];
    assert!(
        msg.starts_with(&format!("Merge #{shorthand}: ")),
        "subject should still be present, got:\n{msg}",
    );
    assert!(
        msg.contains("nostr:nevent1"),
        "nevent reference should still be present, got:\n{msg}",
    );
    assert!(
        !msg.contains("PR description:") && !msg.contains("CoverNote:"),
        "description footer should be omitted with --exclude-description, got:\n{msg}",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 5b. the `PR-Author:` trailer attributes the author by npub and survives
//     --exclude-description (attribution is not free-form description prose).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn author_trailer_carries_author_npub() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        publisher,
    } = setup().await?;
    let pr = &prs[0];

    let out = run_merge(
        &publisher,
        &[&pr.event_id.to_hex(), "--exclude-description"],
    )
    .await?;
    anyhow::ensure!(
        out.status.success(),
        "ngit merge --exclude-description exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let msg = commit_message(&publisher, "main").await?;
    let author_npub = pr
        .author_pubkey
        .to_bech32()
        .context("failed to bech32-encode PR author pubkey")?;

    // The `PR-Author:` label and the author's npub (as a bare `nostr:` URI on
    // its own line) are always present, even with --exclude-description: the
    // attribution is metadata, not the suppressible description prose.
    assert!(
        msg.contains("PR-Author:"),
        "PR-Author trailer should survive --exclude-description, got:\n{msg}",
    );
    assert!(
        msg.contains(&format!("\nnostr:{author_npub}")),
        "author npub should be on its own bare nostr: URI line, got:\n{msg}",
    );

    Ok(())
}
// ---------------------------------------------------------------------------
// 5. merge without id while not on a pr/ branch fails clearly.
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

// ---------------------------------------------------------------------------
// 6. merge without id on a *bare* `pr/<name>` branch the current user pushed
//    themselves (via `git push -u origin pr/<name>`, which has no `(<id>)`
//    shorthand) infers the PR.
//
//    This is the "user submitted their own PR with plain git" workflow: the
//    local branch carries no event-id shorthand, but it is linked to the
//    published PR because the logged-in user authored it. `ngit merge` with no
//    id must resolve it the same way `git-remote-nostr` maps the branch on
//    push (`is_event_proposal_root_for_branch`).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_without_id_infers_self_submitted_bare_pr_branch() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("merge maintainer".into()),
            identifier: Some("merge-bare-branch-repo".into()),
            ..Default::default()
        })
        .await?;

    // The PR author clones and logs in as their own (fresh) account. They are
    // both the author of the PR and the one running `ngit merge` — the only
    // configuration in which a bare `pr/<name>` branch can be linked back to a
    // published PR.
    let author = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "self merging contributor".into(),
            },
        )
        .await?;

    // Submit the PR exactly as a user would: a plain `pr/<name>` branch pushed
    // with `git push -u origin pr/<name>`. `git-remote-nostr` turns this into
    // a KIND_PULL_REQUEST. No `ngit send`, no `(<id>)` shorthand.
    let branch = "pr/my-feature";
    author
        .git_ok(["checkout", "-b", branch], "git checkout -b pr/my-feature")
        .await?;
    std::fs::write(author.dir().join("feat.md"), "some content\n").context("write feat.md")?;
    author.git_ok(["add", "feat.md"], "git add feat.md").await?;
    author
        .git_ok(
            ["commit", "-m", "add feat.md", "--no-gpg-sign"],
            "git commit feat.md",
        )
        .await?;

    let pr_tip = rev_parse(&author, "HEAD").await?;

    author
        .nostr_push(["-u", "origin", branch])
        .await
        .context("git push -u origin pr/my-feature (PR creation) failed")?;

    // Still on the bare `pr/my-feature` branch with no `(<id>)` shorthand.
    assert_eq!(current_branch(&author).await?, branch);

    // Merge with no id — must infer the PR from the self-submitted bare branch.
    let out = run_merge(&author, &[]).await?;
    anyhow::ensure!(
        out.status.success(),
        "ngit merge (no id) on a self-submitted bare pr/ branch exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert_eq!(
        current_branch(&author).await?,
        "main",
        "should be left on the default branch after merge",
    );
    assert_eq!(
        parent_count(&author, "main").await?,
        2,
        "no-ff merge should produce a 2-parent merge commit",
    );

    // the pushed PR tip is a parent of the merge commit (it was merged in)
    let merged_in = rev_parse(&author, "main^2").await?;
    assert_eq!(
        merged_in, pr_tip,
        "the merge commit's second parent should be the pushed PR tip",
    );

    Ok(())
}
