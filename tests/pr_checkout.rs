//! Migrated port of legacy `tests/legacy/ngit_pr_checkout.rs`
//! (5 modules × 2-3 `#[serial]` assertions each = 15 tests).
//!
//! Six `#[tokio::test]`s — one per scenario — each booting its own
//! harness and asserting on refs-on-disk. The legacy file split every
//! scenario across 2-3 sibling tests that shared a per-module
//! `prep_and_run()` via `#[serial]`. In a parallel-by-default world that
//! re-running model is more expensive than collapsing the related
//! assertions into one body, and the assertions in each module are all
//! read-only inspections of the same end state — no rstest fan-out is
//! warranted (see `docs/architecture/test-harness-migration.md`
//! § "rstest discipline").
//!
//! ## Scenario shape (shared by every test)
//!
//! - One vanilla relay (`"default"`) + one GRASP (`"repo"`).
//! - Maintainer publishes the repo via `harness.publish_repo`.
//! - A fresh contributor publishes three PR-kind proposals via
//!   `harness.publish_three_open_proposals` — replacing the legacy
//!   `cli_tester_create_proposals` (which produced patch-kind proposals). The
//!   migration plan endorses this kind-shift: tests here care only that "an
//!   open proposal exists", not which event kind ngit picked for it.
//! - The `test_repo` is a `CloneLogin::None` clone of the published `nostr://`
//!   URL — what real users do (`git clone <nostr-url>`), not the legacy
//!   `GitTestRepo::default()` + `nostr.repo` config shape. The clone gives us
//!   an `origin` remote, a populated `main` matching the publisher's, and
//!   remote-tracking refs for every advertised proposal branch. With no
//!   `nostr.npub` set, `src/bin/git_remote_nostr/list.rs:236` falls through to
//!   the long-form `pr/<branch>(<shorthand>)` ref path; every assertion against
//!   an expected ref builds that long form via [`expected_branch_name`].
//!
//! ## PR-kind branch tracking
//!
//! After `ngit pr checkout` on a PR-kind proposal,
//! `src/bin/ngit/sub_commands/checkout.rs::checkout_pr` always wires
//! the local branch's upstream to the nostr remote (writing
//! `refs/remotes/<remote>/<pr-branch>` at the checked-out tip and
//! calling `set_upstream`) so that the user can `git pull` later.
//! That's true regardless of whether the test_repo was cloned via
//! `git clone <nostr-url>` (which would have pre-populated those
//! remote-tracking refs) or just had the remote added afterwards.
//!
//! Successive `ngit pr checkout` invocations against the same PR
//! follow the regular Cases 2-5 in `checkout_pr` (up-to-date,
//! fast-forward, local-commits-on-top, diverged) — no upstream-set
//! short-circuit. The complementary `tests/pr_checkout_patch.rs`
//! re-asserts the same end-state contract against patch-kind
//! proposals.
//!
//! ## Event ids straight off `PublishedPr`
//!
//! Legacy `use_ngit_pr_checkout` round-tripped through
//! `ngit pr list --json` to discover proposal event ids; our scenario
//! builder hands them back on [`PublishedPr::event_id`], so each
//! `ngit pr checkout` call passes a hex string directly. One less
//! subprocess + one less JSON parse per assertion, and the test reads
//! linearly rather than as a discovery dance.
//!
//! ## Why no `--offline`
//!
//! Legacy ran `ngit pr list` once to populate the cache, then used
//! `--offline` on every subsequent checkout to avoid a second relay
//! round-trip. Here the relays + grasp are alive for the whole test, so
//! `ngit pr checkout` does its own fetch and we get the same end-state
//! without the cache-priming dance.

use anyhow::{Context, Result};
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedPr, PublishedRepo, Repo};

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

struct Setup {
    /// Kept alive only so relays + grasp keep serving REQs / git smart-http
    /// requests for the duration of the test. Several tests also reach back
    /// into it for `clone_published_repo` to spin up a second clone.
    harness: Harness,
    _published: PublishedRepo,
    prs: [PublishedPr; 3],
    test_repo: Repo,
    /// Maintainer clone — kept so the revision scenario can publish a
    /// rebased revision as the maintainer (who is in `permissioned_users`
    /// and therefore visible to
    /// `get_all_proposal_patch_pr_pr_update_events_from_cache`).
    publisher: Repo,
}

/// One harness boot + three PR-kind proposals + a fresh logged-out
/// `test_repo`. Each test calls this directly; nothing is `#[once]` because
/// the relays + grasp instances are per-harness anyway.
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
            display_name: Some("pr-checkout maintainer".into()),
            identifier: Some("pr-checkout-repo".into()),
            ..Default::default()
        })
        .await?;

    let prs = harness.publish_three_open_proposals(&published).await?;

    // CloneLogin::None: no `ngit account create` / `account login`, so
    // `nostr.npub` stays unset in the cloned repo's local config.
    // `src/lib/login/mod.rs:114 get_curent_user` then returns None, and
    // `src/bin/git_remote_nostr/list.rs:236` falls through to the
    // long-form `pr/<branch>(<shorthand>)` ref name — matching the
    // legacy `GitTestRepo::default()` test_repo's behaviour.
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    Ok(Setup {
        harness,
        _published: published,
        prs,
        test_repo,
        publisher,
    })
}

/// Long-form `pr/<branch>(<root-event-id-shorthand>)` — what
/// `CoverLetter::get_branch_name_with_pr_prefix_and_shorthand_id` builds in
/// `src/lib/git_events.rs:805` for proposals whose author doesn't match
/// the current user. Always the path taken with our `CloneLogin::None`
/// test repo.
fn expected_branch_name(pr: &PublishedPr) -> String {
    let hex = pr.event_id.to_hex();
    format!("pr/{}({})", pr.branch_name, &hex[..8])
}

// ---------------------------------------------------------------------------
// Subprocess helpers
// ---------------------------------------------------------------------------

/// `ngit pr checkout <hex>`. Returns Ok only on clean exit; `Err` carries
/// captured stdout/stderr so tests asserting `.is_err()` can still surface
/// the cause when they actually expected success.
async fn run_pr_checkout(repo: &Repo, pr: &PublishedPr) -> Result<()> {
    let event_id_hex = pr.event_id.to_hex();
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

/// `ngit pr checkout --force <hex>`.
async fn run_pr_checkout_force(repo: &Repo, pr: &PublishedPr) -> Result<()> {
    let event_id_hex = pr.event_id.to_hex();
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

/// Run an arbitrary `git` subcommand inside `repo`, bailing with captured
/// stdout/stderr on non-zero exit. Centralises the verbose error wrapping
/// so test bodies stay readable.
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

/// `git rev-parse <rev>` — resolve `<rev>` to a full commit oid.
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

/// `git symbolic-ref --short HEAD` — currently checked-out branch name.
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

/// `git merge-base --is-ancestor <maybe-ancestor> <descendant>` — exit
/// status 0 means `maybe_ancestor` is an ancestor of `descendant`.
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

/// Folds the legacy `proposal_branch_created_with_correct_name` +
/// `proposal_branch_checked_out` + `proposal_branch_tip_is_most_recent_patch`
/// trio into one body. All three were read-only inspections of the same
/// post-checkout state; running the shared setup three times under
/// `#[serial]` is exactly what the migration aims to drop.
#[tokio::test]
async fn fresh_branch_is_created_checked_out_and_at_published_tip() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        test_repo,
        publisher: _,
    } = setup().await?;
    let pr = &prs[0];

    run_pr_checkout(&test_repo, pr).await?;

    let branch = expected_branch_name(pr);

    // (a) branch exists with the expected long-form name — assertion is
    //     implicit in `rev_parse` succeeding below, but we also assert
    //     `current_branch` for an explicit name check.
    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "expected the proposal branch to be the currently checked-out one",
    );

    // (b) tip equals the published PR tip
    let tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip, pr.tip,
        "proposal branch tip should match the published PR's tip",
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
        prs,
        test_repo,
        publisher: _,
    } = setup().await?;
    let pr = &prs[0];

    run_pr_checkout(&test_repo, pr).await?;
    let branch = expected_branch_name(pr);
    let first_tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(first_tip, pr.tip);

    git_ok(&test_repo, ["checkout", "main"], "git checkout main").await?;

    // second invocation is the up-to-date code path
    // (`src/bin/ngit/sub_commands/checkout.rs:238`).
    run_pr_checkout(&test_repo, pr).await?;

    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "second checkout should leave us on the proposal branch",
    );
    let second_tip = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        second_tip, pr.tip,
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
        prs,
        test_repo,
        publisher: _,
    } = setup().await?;
    let pr = &prs[0];

    run_pr_checkout(&test_repo, pr).await?;
    let branch = expected_branch_name(pr);

    // Rewind the local branch to its parent so it sits one commit behind
    // the published tip. Equivalent of legacy
    // `remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main`.
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
    assert_eq!(
        rewound_tip, parent_oid,
        "branch should now point at its previous parent commit",
    );
    assert_ne!(
        rewound_tip, pr.tip,
        "branch should differ from published tip after the rewind",
    );

    // Re-checkout — the fast-forward path
    // (`checkout.rs:267 local_is_ancestor_of_published`) should restore
    // the published tip.
    run_pr_checkout(&test_repo, pr).await?;

    assert_eq!(current_branch(&test_repo).await?, branch);
    let tip_after = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip_after, pr.tip,
        "branch should have been fast-forwarded back to the published tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 4 — legacy `when_proposal_branch_has_local_amendments` (1)
// ---------------------------------------------------------------------------

/// Locally amend the proposal tip (rewind + add a different commit) so the
/// branch is diverged from the published tip. `ngit pr checkout` without
/// `--force` should bail and leave the local amendment intact.
#[tokio::test]
async fn local_amendments_are_preserved_when_checkout_without_force_fails() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        test_repo,
        publisher: _,
    } = setup().await?;
    let pr = &prs[0];

    run_pr_checkout(&test_repo, pr).await?;
    let branch = expected_branch_name(pr);

    // Rewind one commit then add a different commit on the branch —
    // equivalent of legacy `amend_last_commit`.
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
    assert_ne!(amended_tip, pr.tip);
    assert_ne!(amended_tip, parent_oid);
    git_ok(
        &test_repo,
        ["checkout", "main"],
        "git checkout main (post-amend)",
    )
    .await?;

    // `ngit pr checkout` should refuse to overwrite the diverged branch
    // without `--force` (checkout.rs:288 fall-through path).
    let res = run_pr_checkout(&test_repo, pr).await;
    assert!(
        res.is_err(),
        "expected `ngit pr checkout` to fail on a diverged branch without --force",
    );

    // The amended local commit must survive the failed checkout.
    let tip_after = rev_parse(&test_repo, &branch).await?;
    assert_eq!(
        tip_after, amended_tip,
        "amended local commit should still be the branch tip",
    );
    assert_ne!(
        tip_after, pr.tip,
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
        prs,
        test_repo,
        publisher: _,
    } = setup().await?;
    let pr = &prs[0];

    run_pr_checkout(&test_repo, pr).await?;
    let branch = expected_branch_name(pr);

    // Add an extra commit on top of the proposal branch.
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
    assert_ne!(local_tip, pr.tip);
    git_ok(
        &test_repo,
        ["checkout", "main"],
        "git checkout main (between checkouts)",
    )
    .await?;

    // Second checkout — published tip is an ancestor of the local tip
    // (`checkout.rs:276 published_is_ancestor_of_local`), so the branch
    // is checked out without touching the ref.
    run_pr_checkout(&test_repo, pr).await?;

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
        tip_after, pr.tip,
        "local tip should still be ahead of the published tip",
    );
    assert!(
        is_ancestor(&test_repo, &pr.tip, &tip_after).await?,
        "published tip must be an ancestor of the local tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 6 — legacy `when_newer_revision_rebases_proposal` (2)
// ---------------------------------------------------------------------------

/// Drives the revision manually: a second contributor clone adds a local
/// "rebase base" commit on main, branches off it for the feature, and
/// runs `ngit send --force-pr --in-reply-to <orig>`. With the original
/// proposal being a `KIND_PULL_REQUEST`, the resulting event becomes a
/// `KIND_PULL_REQUEST_UPDATE` (see `src/lib/git_events.rs:551
/// is_pr_update`).
///
/// Drives the revision manually: the maintainer (`publisher` from
/// `setup()`) adds a local "rebase base" commit on main, branches off it
/// for the feature, and runs `ngit send --force-pr --in-reply-to <orig>`.
/// With the original proposal being a `KIND_PULL_REQUEST`, the resulting
/// event becomes a `KIND_PULL_REQUEST_UPDATE` (see
/// `src/lib/git_events.rs:551 is_pr_update`).
///
/// The revision is published by the maintainer (not a fresh contributor)
/// to satisfy the `permissioned_users` filter in
/// `get_all_proposal_patch_pr_pr_update_events_from_cache` (which keeps
/// only events authored by maintainers or the original proposal author).
/// A third-party contributor would be filtered out and the second
/// `ngit pr checkout` would see only the original PR event, incorrectly
/// returning "up-to-date".
///
/// We cannot reuse `Harness::publish_pr` for this because it filters its
/// verification query on `kind(KIND_PULL_REQUEST)` only — a `PR_UPDATE`
/// would fail that probe. Inlining the revision here keeps PR 2 free of
/// new scenario builders, as the migration plan prescribes.
#[tokio::test]
async fn newer_revision_force_updates_to_revised_tip() -> Result<()> {
    let Setup {
        harness: _h,
        _published: _,
        prs,
        test_repo,
        publisher,
    } = setup().await?;
    let pr = &prs[0];

    // (1) initial checkout — branch lands at the original published tip.
    run_pr_checkout(&test_repo, pr).await?;
    let branch = expected_branch_name(pr);
    let tip_before_revision = rev_parse(&test_repo, &branch).await?;
    assert_eq!(tip_before_revision, pr.tip);

    // (2) Publisher (maintainer) publishes a rebased revision. Using the
    //     maintainer ensures the revision PR_UPDATE passes the
    //     `permissioned_users` filter and is visible to
    //     `get_all_proposal_patch_pr_pr_update_events_from_cache`.
    //
    // local "rebase base" commit on main — mirrors legacy
    // `create_proposals_with_rebased_first_proposal`'s "commit for
    // rebasing on top of".
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
    // Push the rebase-base commit to origin so the test_repo's
    // `ensure_commit_local` (driven from `checkout_pr` --force) can walk
    // back from the revision's tip and find a known ancestor.
    publisher.nostr_push(["origin", "main"]).await?;

    // Use the same branch name as the original proposal so the
    // revision's `branch-name` tag matches and `list.rs` groups it under
    // the same proposal address.
    git_ok(
        &publisher,
        ["checkout", "-b", &pr.branch_name],
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
    assert_ne!(revised_tip, pr.tip);

    let in_reply_to_hex = pr.event_id.to_hex();
    let send_out = publisher
        .ngit([
            "send",
            "HEAD~2",
            "--force-pr",
            "--force",
            "--defaults",
            "--in-reply-to",
            &in_reply_to_hex,
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-pr --in-reply-to")?;
    anyhow::ensure!(
        send_out.status.success(),
        "ngit send revision exited {:?}\nstdout: {}\nstderr: {}",
        send_out.status,
        String::from_utf8_lossy(&send_out.stdout),
        String::from_utf8_lossy(&send_out.stderr),
    );

    // (3) test_repo refuses to overwrite without `--force` — branch is
    //     now diverged from the freshly-published revision tip.
    let res = run_pr_checkout(&test_repo, pr).await;
    assert!(
        res.is_err(),
        "expected `ngit pr checkout` to fail on a diverged branch without --force after revision",
    );

    // (4) `--force` updates the branch to the revision's tip.
    run_pr_checkout_force(&test_repo, pr).await?;
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
