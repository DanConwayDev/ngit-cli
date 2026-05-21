//! Patch-kind regression coverage for `ngit pr checkout`, complementing
//! the PR-kind suite in `tests/pr_checkout.rs`.
//!
//! Same 6 scenarios as the PR-kind file — fresh branch, up-to-date,
//! behind, local amendments, local commits on top, and a newer revision
//! rebasing the proposal — but driven through patch-kind proposals via
//! [`Harness::publish_three_open_patch_proposals`]. Patch-kind is the
//! shape every legacy `ngit_pr_checkout` test used (because
//! `cli_tester_create_proposals` published cover-lettered patches), so
//! keeping these here preserves regression coverage of
//! `checkout_patch`'s case-3/4/5 semantics:
//!
//! - **Case 3 — behind**: local tip is an ancestor of the published tip →
//!   fast-forward via `apply_patch_chain`.
//! - **Case 4 — commits on top**: published tip is an ancestor of the local tip
//!   → check out without touching the ref.
//! - **Case 5 — diverged**: neither is an ancestor of the other → without
//!   `--force`, bail with the diverged-help message; with `--force`, overwrite
//!   via `apply_patch_chain`.
//!
//! ## Why this file exists alongside `pr_checkout.rs`
//!
//! `checkout_pr` (PR-kind) adds an upstream-deferral short-circuit at
//! `src/bin/ngit/sub_commands/checkout.rs:247` that `checkout_patch`
//! does not have: once a local PR branch has any upstream set,
//! subsequent `ngit pr checkout` invocations return Ok with "Run git
//! pull to update." instead of going down case-3/4/5. A real cloned
//! `test_repo` always ends up with upstream set after the first
//! checkout because `checkout_remote_branch_with_tracking` at
//! `checkout.rs:223` wires it. The PR-kind suite therefore `#[ignore]`s
//! the three cases that hit the deferral path; this file picks up the
//! slack by exercising the patch-kind path where the legacy assertions
//! still hold. See `pr_checkout.rs`'s module-level doc-comment for the
//! full write-up.
//!
//! ## Shape (shared by every test)
//!
//! - One vanilla relay (`"default"`) + one GRASP (`"repo"`).
//! - Maintainer publishes the repo via `harness.publish_repo`.
//! - A fresh contributor publishes three patch-kind proposals (each a 2-commit
//!   series with a cover letter) via
//!   `harness.publish_three_open_patch_proposals`.
//! - `test_repo` = `CloneLogin::None` clone of the published `nostr://` URL —
//!   what a real user does (`git clone <nostr-url>`). With no `nostr.npub` set,
//!   the long-form `pr/<branch>(<shorthand>)` ref name is what gets advertised
//!   by `src/bin/git_remote_nostr/list.rs:236`; assertions build that form via
//!   [`expected_branch_name`].
//!
//! ## Why no `--offline`
//!
//! Legacy primed the cache via `ngit pr list` and used `--offline` on
//! every checkout. Here the relays + grasp are alive throughout, so
//! `ngit pr checkout`'s own fetch is fast and avoids the cache-priming
//! dance. The end state is the same.

use anyhow::{Context, Result};
use test_harness::{
    CloneLogin, Harness, PublishRepoOpts, PublishedPatchSeries, PublishedRepo, Repo,
};

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

struct Setup {
    /// Kept alive only so relays + grasp keep accepting REQs / git
    /// smart-http requests for the test's lifetime. The revision scenario
    /// also reaches back in for a second `clone_published_repo`.
    harness: Harness,
    _published: PublishedRepo,
    proposals: [PublishedPatchSeries; 3],
    test_repo: Repo,
    /// Maintainer clone — kept so the revision scenario can publish a
    /// rebased revision as the maintainer (who is in `permissioned_users`
    /// and therefore visible to
    /// `get_all_proposal_patch_pr_pr_update_events_from_cache`).
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
            display_name: Some("pr-checkout-patch maintainer".into()),
            identifier: Some("pr-checkout-patch-repo".into()),
            ..Default::default()
        })
        .await?;

    let proposals = harness
        .publish_three_open_patch_proposals(&published)
        .await?;

    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    Ok(Setup {
        harness,
        _published: published,
        proposals,
        test_repo,
        publisher,
    })
}

/// Long-form `pr/<branch>(<root-event-id-shorthand>)` — the format
/// `src/lib/git_events.rs:805 get_branch_name_with_pr_prefix_and_shorthand_id`
/// produces for proposals whose author doesn't match the current user
/// (always the case with our `CloneLogin::None` test repo).
///
/// For patch-kind, the `event_id` we shorthand off is the cover-letter
/// patch event (the "root" of the series), since
/// `event_to_cover_letter` is what `list.rs` calls to derive the name.
fn expected_branch_name(series: &PublishedPatchSeries) -> String {
    let root = series
        .cover_letter_event
        .as_ref()
        .expect("publish_three_open_patch_proposals always emits a cover letter");
    let hex = root.id.to_hex();
    format!("pr/{}({})", series.branch_name, &hex[..8])
}

/// The event id we feed to `ngit pr checkout` — the cover-letter patch's
/// id, since that's what `pr list` advertises as the proposal's id and
/// what `checkout.rs::launch` looks up in the cache.
fn root_event_id_hex(series: &PublishedPatchSeries) -> String {
    series
        .cover_letter_event
        .as_ref()
        .expect("publish_three_open_patch_proposals always emits a cover letter")
        .id
        .to_hex()
}

// ---------------------------------------------------------------------------
// Subprocess helpers (lift-and-shift of `tests/pr_checkout.rs` shape)
// ---------------------------------------------------------------------------

async fn run_pr_checkout(repo: &Repo, series: &PublishedPatchSeries) -> Result<()> {
    let event_id_hex = root_event_id_hex(series);
    let out = repo
        .ngit(["pr", "checkout", &event_id_hex])
        .output()
        .await
        .context("failed to spawn ngit pr checkout")?;
    anyhow::ensure!(
        out.status.success(),
        "ngit pr checkout {event_id_hex} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

async fn run_pr_checkout_force(repo: &Repo, series: &PublishedPatchSeries) -> Result<()> {
    let event_id_hex = root_event_id_hex(series);
    let out = repo
        .ngit(["pr", "checkout", "--force", &event_id_hex])
        .output()
        .await
        .context("failed to spawn ngit pr checkout --force")?;
    anyhow::ensure!(
        out.status.success(),
        "ngit pr checkout --force {event_id_hex} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
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

async fn is_ancestor(repo: &Repo, maybe_ancestor: &str, descendant: &str) -> Result<bool> {
    let out = repo
        .git(["merge-base", "--is-ancestor", maybe_ancestor, descendant])
        .output()
        .await
        .context("failed to spawn git merge-base --is-ancestor")?;
    Ok(out.status.success())
}

// ---------------------------------------------------------------------------
// Scenario 1 — legacy `when_proposal_branch_doesnt_exist` (3 assertions)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fresh_branch_is_created_checked_out_and_at_published_tip() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        proposals,
        test_repo,
        publisher: _,
    } = setup().await?;
    let series = &proposals[0];

    run_pr_checkout(&test_repo, series).await?;

    let branch = expected_branch_name(series);

    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "expected the proposal branch to be the currently checked-out one",
    );

    let tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip, series.tip,
        "proposal branch tip should match the published patch series' tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 2 — legacy `when_proposal_branch_exists_and_is_up_to_date` (2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn up_to_date_branch_stays_at_published_tip_after_second_checkout() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        proposals,
        test_repo,
        publisher: _,
    } = setup().await?;
    let series = &proposals[0];

    run_pr_checkout(&test_repo, series).await?;
    let branch = expected_branch_name(series);
    let first_tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(first_tip, series.tip);

    git_ok(&test_repo, ["checkout", "main"], "git checkout main").await?;

    // Second invocation should land on `checkout_patch` Case 2 (already
    // up to date) — branch checks out clean, tip unchanged.
    run_pr_checkout(&test_repo, series).await?;

    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "second checkout should leave us on the proposal branch",
    );
    let second_tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        second_tip, series.tip,
        "tip should still equal the published tip after a no-op checkout",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 3 — legacy `when_proposal_branch_exists_and_is_behind` (2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn behind_branch_fast_forwards_to_published_tip() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        proposals,
        test_repo,
        publisher: _,
    } = setup().await?;
    let series = &proposals[0];

    run_pr_checkout(&test_repo, series).await?;
    let branch = expected_branch_name(series);

    // Rewind one commit so the branch is behind the published tip —
    // legacy `remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main`.
    git_ok(
        &test_repo,
        ["checkout", "main"],
        "git checkout main (pre-rewind)",
    )
    .await?;
    let parent_oid = rev_parse(&test_repo, &format!("{branch}~1")).await?;
    git_ok(
        &test_repo,
        ["branch", "-f", &branch, &parent_oid],
        "git branch -f <branch> <branch>~1",
    )
    .await?;
    let rewound_tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(rewound_tip, parent_oid);
    assert_ne!(rewound_tip, series.tip);

    // Second checkout hits `checkout_patch` Case 3 (behind) — replays
    // the patch chain so the local tip catches up to the published tip.
    run_pr_checkout(&test_repo, series).await?;

    assert_eq!(current_branch(&test_repo).await?, branch);
    let tip_after = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip_after, series.tip,
        "branch should have been fast-forwarded back to the published tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 4 — legacy `when_proposal_branch_has_local_amendments` (1)
// ---------------------------------------------------------------------------

/// `checkout_patch` Case 5 (diverged): without `--force` the call bails;
/// the local amendment is preserved.
#[tokio::test]
async fn local_amendments_are_preserved_when_checkout_without_force_fails() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        proposals,
        test_repo,
        publisher: _,
    } = setup().await?;
    let series = &proposals[0];

    run_pr_checkout(&test_repo, series).await?;
    let branch = expected_branch_name(series);

    // Rewind one commit then add a different commit on the branch —
    // legacy `amend_last_commit`.
    git_ok(
        &test_repo,
        ["checkout", "main"],
        "git checkout main (pre-amend)",
    )
    .await?;
    let parent_oid = rev_parse(&test_repo, &format!("{branch}~1")).await?;
    git_ok(
        &test_repo,
        ["branch", "-f", &branch, &parent_oid],
        "git branch -f rewind (amend setup)",
    )
    .await?;
    git_ok(&test_repo, ["checkout", &branch], "git checkout <branch>").await?;
    std::fs::write(
        test_repo.dir().join("ammended-commit.md"),
        "add ammended-commit.md",
    )
    .context("write ammended-commit.md")?;
    git_ok(&test_repo, ["add", "ammended-commit.md"], "git add amend").await?;
    git_ok(
        &test_repo,
        ["commit", "-m", "add ammended-commit.md", "--no-gpg-sign"],
        "git commit amend",
    )
    .await?;
    let amended_tip = rev_parse(&test_repo, &branch).await?;
    assert_ne!(amended_tip, series.tip);
    assert_ne!(amended_tip, parent_oid);
    git_ok(
        &test_repo,
        ["checkout", "main"],
        "git checkout main (post-amend)",
    )
    .await?;

    // Without --force `checkout_patch` Case 5 should bail.
    let res = run_pr_checkout(&test_repo, series).await;
    assert!(
        res.is_err(),
        "expected `ngit pr checkout` to fail on a diverged branch without --force",
    );

    // Amended local commit must survive the failed checkout.
    let tip_after = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip_after, amended_tip,
        "amended local commit should still be the branch tip",
    );
    assert_ne!(
        tip_after, series.tip,
        "local tip must NOT have been overwritten with the published tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 5 — legacy `when_proposal_branch_has_local_commits_on_top` (2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_commits_on_top_are_not_discarded() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        proposals,
        test_repo,
        publisher: _,
    } = setup().await?;
    let series = &proposals[0];

    run_pr_checkout(&test_repo, series).await?;
    let branch = expected_branch_name(series);

    git_ok(&test_repo, ["checkout", &branch], "git checkout <branch>").await?;
    std::fs::write(test_repo.dir().join("local-extra.md"), "local work\n")
        .context("write local-extra.md")?;
    git_ok(&test_repo, ["add", "local-extra.md"], "git add local-extra").await?;
    git_ok(
        &test_repo,
        ["commit", "-m", "add local-extra.md", "--no-gpg-sign"],
        "git commit local-extra",
    )
    .await?;
    let local_tip = rev_parse(&test_repo, &branch).await?;
    assert_ne!(local_tip, series.tip);
    git_ok(
        &test_repo,
        ["checkout", "main"],
        "git checkout main (between checkouts)",
    )
    .await?;

    // `checkout_patch` Case 4 (published_is_ancestor_of_local) — branch
    // checks out without touching the ref.
    run_pr_checkout(&test_repo, series).await?;

    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "branch should be checked out after the second `ngit pr checkout`",
    );
    let tip_after = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip_after, local_tip,
        "local commits should not have been discarded",
    );
    assert_ne!(
        tip_after, series.tip,
        "local tip should still be ahead of the published tip",
    );
    assert!(
        is_ancestor(&test_repo, &series.tip, &tip_after).await?,
        "published tip must be an ancestor of the local tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 6 — legacy `when_newer_revision_rebases_proposal` (2)
// ---------------------------------------------------------------------------

/// A second contributor publishes a rebased patch-series revision via
/// `ngit send --force-patch --no-cover-letter --in-reply-to <orig>`.
/// `is_pr_update` in `src/lib/git_events.rs:551` only fires when the
/// root proposal is a PR-kind; for patch-kind it stays `false`, so the
/// revision events come out as plain `Kind::GitPatch` patches and
/// `checkout_patch` is the path that handles them. Without `--force`
/// the second `ngit pr checkout` should bail (Case 5: diverged); with
/// `--force` it overwrites the branch via `apply_patch_chain`.
///
/// Inlined rather than exposed as a scenario builder because (a) it
/// recurs in only one test and (b) any builder would need to query the
/// new root patch event by branch-name tag to return a useful handle —
/// pulling that out is more boilerplate than the test body itself.
///
/// The revision is published by the maintainer (the `publisher` clone from
/// `setup()`). Using the maintainer ensures the revision patches pass the
/// `permissioned_users` filter in
/// `get_all_proposal_patch_pr_pr_update_events_from_cache` (which allows
/// maintainers and the original proposal author). A third-party contributor
/// would be filtered out and the second `ngit pr checkout` would see only
/// the original patches, incorrectly returning "up-to-date".
#[tokio::test]
async fn newer_revision_force_updates_to_revised_tip() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        proposals,
        test_repo,
        publisher,
    } = setup().await?;
    let series = &proposals[0];

    // (1) Initial checkout — branch lands at the original published tip.
    run_pr_checkout(&test_repo, series).await?;
    let branch = expected_branch_name(series);
    let tip_before_revision = rev_parse(&test_repo, &branch).await?;
    assert_eq!(tip_before_revision, series.tip);

    // (2) Publisher (maintainer) publishes a rebased revision.
    //     Using the maintainer ensures the revision patches are visible to
    //     `get_all_proposal_patch_pr_pr_update_events_from_cache`.
    std::fs::write(publisher.dir().join("amazing.md"), "rebase base content\n")
        .context("write amazing.md")?;
    git_ok(&publisher, ["add", "amazing.md"], "git add amazing.md").await?;
    git_ok(
        &publisher,
        [
            "commit",
            "-m",
            "commit for rebasing on top of",
            "--no-gpg-sign",
        ],
        "git commit rebase base",
    )
    .await?;
    // Push the "rebase base" commit to origin so the test_repo can fetch it
    // when `ngit pr checkout --force` needs to apply the revision patches.
    publisher.nostr_push(["origin", "main"]).await?;
    git_ok(
        &publisher,
        ["checkout", "-b", &series.branch_name],
        "git checkout -b feature (revision)",
    )
    .await?;
    std::fs::write(publisher.dir().join("revised-a3.md"), "revised a3\n")
        .context("write revised-a3.md")?;
    git_ok(&publisher, ["add", "revised-a3.md"], "git add revised-a3").await?;
    git_ok(
        &publisher,
        ["commit", "-m", "add revised-a3.md", "--no-gpg-sign"],
        "git commit revised-a3",
    )
    .await?;
    std::fs::write(publisher.dir().join("revised-a4.md"), "revised a4\n")
        .context("write revised-a4.md")?;
    git_ok(&publisher, ["add", "revised-a4.md"], "git add revised-a4").await?;
    git_ok(
        &publisher,
        ["commit", "-m", "add revised-a4.md", "--no-gpg-sign"],
        "git commit revised-a4",
    )
    .await?;
    let revised_tip = rev_parse(&publisher, "HEAD").await?;
    assert_ne!(revised_tip, series.tip);

    let in_reply_to_hex = root_event_id_hex(series);
    let send_out = publisher
        .ngit([
            "send",
            "HEAD~2",
            "--force-patch",
            "--no-cover-letter",
            "--in-reply-to",
            &in_reply_to_hex,
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-patch --in-reply-to")?;
    anyhow::ensure!(
        send_out.status.success(),
        "ngit send revision exited {:?}\nstdout: {}\nstderr: {}",
        send_out.status,
        String::from_utf8_lossy(&send_out.stdout),
        String::from_utf8_lossy(&send_out.stderr),
    );

    // (3) Without --force the branch must NOT be overwritten —
    //     `checkout_patch` Case 5 bails.
    let res = run_pr_checkout(&test_repo, series).await;
    assert!(
        res.is_err(),
        "expected `ngit pr checkout` to fail on a diverged branch without --force after revision",
    );

    // (4) --force overwrites via `apply_patch_chain`.
    run_pr_checkout_force(&test_repo, series).await?;
    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "branch should be checked out after the force-checkout",
    );
    let tip_after = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip_after, revised_tip,
        "branch should now point at the revision's tip",
    );

    Ok(())
}
