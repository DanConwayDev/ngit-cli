//! Non-interactive `ngit send` regression coverage — final migration step for
//! `tests/legacy/ngit_send.rs`. Covers:
//!
//! - clap arg-combo validation (rstest) — successor to legacy
//!   `non_interactive_validation::{bare_send_errors_with_helpful_message,
//!   send_with_range_only_errors, send_force_pr_without_title_errors,
//!   send_description_without_title_errors,
//!   send_title_without_description_errors}`.
//! - `--defaults` flow publishes patches without a cover letter — successor to
//!   `non_interactive_validation::send_defaults_sends_patches_without_cover_letter`.
//! - missing default branch errors — successor to
//!   `when_no_main_or_master_branch_return_error`.
//! - commits-behind-main: without `--force` errors with a recommendation; with
//!   `--force` succeeds and publishes patches. Non-interactive replacement for
//!   the entire `when_commits_behind_ask_to_proceed` module, whose three legacy
//!   tests (`asked_with_default_no`, `when_response_is_false_aborts`,
//!   `when_response_is_true_proceeds`) all drove the dialoguer confirm prompt
//!   and are banned by the harness rules in `docs/architecture/test-harness.md`
//!   § "Anti-patterns". The behaviour under test — "behind detection bails
//!   helpfully and `--force` overrides" — is exercised here against the
//!   non-interactive code path in `check_commits_are_suitable_for_proposal`
//!   (`src/bin/ngit/sub_commands/send.rs:505-523`).
//!
//! ## Dropped from legacy in this PR (per harness rules)
//!
//! - `when_commits_behind_ask_to_proceed::*` (3 tests) — pure dialoguer prompt
//!   rendering; replaced by the non-interactive `--force` flow tests in this
//!   file.
//! - `when_cover_letter_details_specified_*::cli_ouput::check_cli_output`,
//!   `when_cover_letter_details_specified_*::first_event_rejected_by_1_relay::*
//!   ` `when_range_ommited::cli_ouput::check_cli_output` — exact-stdout
//!   rendering banned by harness rules.
//! - `when_range_ommited::two_patch_events_sent` — depends on the interactive
//!   multi-choice commit selector. The non-interactive equivalent is
//!   `--defaults`, covered here by
//!   `send_with_defaults_publishes_patches_without_cover_letter`.
//! - `when_cover_letter_details_specified_*` relay-distribution group (6 tests)
//!   — every patch event published lands on the grasp's repo-relay because
//!   `publish_patch_series` itself REQs the grasp post-send and bails when the
//!   expected events aren't present (`test_harness/src/scenarios.rs:915-988`).
//!   The fanout-to-multiple-relays angle these tests originally captured is
//!   properly the domain of the multi-grasp helper landing in PR 5a of the
//!   migration plan (see `docs/architecture/test-harness-migration.md`); doing
//!   it here would duplicate that work against a single grasp.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{CloneLogin, Harness, PublishRepoOpts, Repo};
// ---------------------------------------------------------------------------
// arg-combo validation (rstest)
// ---------------------------------------------------------------------------

/// One arg-combo to feed `ngit send`, plus the substrings the error must
/// contain. Captures the contract `validate_send_args` enforces in
/// `src/bin/ngit/sub_commands/send.rs:70-133`.
#[derive(Debug, Clone)]
struct ValidationCase {
    /// Args after the `send` subcommand.
    args: &'static [&'static str],
    /// Substrings the combined stdout+stderr must contain. Lower-cased on both
    /// sides — `cli_error` styles its output but the underlying text is
    /// case-stable.
    expected: &'static [&'static str],
}

#[rstest]
#[case::bare_send(ValidationCase {
    args: &[],
    expected: &[
        "ngit send requires additional arguments",
        "<since_or_range>",
        "--subject",
        "--description",
        "--defaults",
        "--interactive",
    ],
})]
#[case::range_only(ValidationCase {
    args: &["HEAD~2"],
    expected: &[
        "ngit send requires additional arguments",
        "--subject",
        "--description",
        "--defaults",
    ],
})]
#[case::force_pr_without_title(ValidationCase {
    args: &["--force-pr", "HEAD~2"],
    expected: &[
        "ngit send requires additional arguments",
        "--subject",
        "--description",
        "--defaults",
    ],
})]
#[case::description_without_subject(ValidationCase {
    args: &["--description", "Y", "HEAD~2"],
    expected: &[
        "ngit send requires --subject when --description is provided",
        "--subject",
    ],
})]
#[case::subject_without_description(ValidationCase {
    args: &["--subject", "X", "HEAD~2"],
    expected: &[
        "ngit send requires --description when --subject is provided",
        "--description",
    ],
})]
#[tokio::test]
async fn non_interactive_arg_validation_errors(#[case] case: ValidationCase) -> Result<()> {
    // No relay / grasp roster: `validate_send_args` runs before any client
    // setup so these cases never touch the network. Spinning up a relay would
    // multiply the rstest startup cost five-fold for no behavioural benefit.
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;

    let repo = harness.fresh_repo()?;
    seed_main_commit(&repo).await?;

    let mut argv: Vec<&str> = vec!["send"];
    argv.extend(case.args.iter().copied());
    let out = repo
        .ngit(&argv)
        .output()
        .await
        .context("failed to spawn ngit send")?;

    if out.status.success() {
        bail!(
            "ngit send {:?} unexpectedly succeeded\nstdout: {}\nstderr: {}",
            case.args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // `cli_error` writes the message to stderr; the legacy PTY reader merged
    // both streams, so check the union for substring matches.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .to_ascii_lowercase();
    for expected in case.expected {
        let needle = expected.to_ascii_lowercase();
        assert!(
            combined.contains(&needle),
            "ngit send {:?} output missing expected substring {expected:?}\ncombined output:\n{combined}",
            case.args,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// missing default branch
// ---------------------------------------------------------------------------

/// `ngit send` short-circuits when neither `main` nor `master` exists — the
/// `get_main_or_master_branch` call at the very top of
/// `src/bin/ngit/sub_commands/send.rs:140-142` bails before any other work.
/// Legacy covered this in `when_no_main_or_master_branch_return_error`.
#[tokio::test]
async fn send_errors_when_no_main_or_master_branch_exists() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;

    // `fresh_repo` initialises with `main` as the default branch — we need a
    // non-`main`/`master` branch to exercise the error. Switch the working
    // tree onto `notmain` and commit there so HEAD points at a branch that
    // doesn't trigger `get_main_or_master_branch`'s success path.
    let repo = harness.fresh_repo()?;
    check_ok(
        "git checkout -b notmain",
        repo.git(["checkout", "-b", "notmain"])
            .output()
            .await
            .context("failed to spawn git checkout -b notmain")?,
    )?;
    seed_commit(&repo, "README.md", "hello").await?;

    let out = repo
        .ngit(["send"])
        .output()
        .await
        .context("failed to spawn ngit send")?;
    if out.status.success() {
        bail!(
            "ngit send unexpectedly succeeded without a main/master branch\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .to_ascii_lowercase();
    assert!(
        combined.contains("default branches")
            && combined.contains("main")
            && combined.contains("master"),
        "expected stderr to mention the default-branch error; got:\n{combined}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// --defaults flow
// ---------------------------------------------------------------------------

/// Running `ngit send --defaults` on a feature branch (no `--subject`, no
/// `--description`, no range) auto-selects the commits ahead of `main` and
/// publishes them as patches *without* a cover letter. Successor to
/// `non_interactive_validation::send_defaults_sends_patches_without_cover_letter`.
#[tokio::test]
async fn send_with_defaults_publishes_patches_without_cover_letter() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let clone = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "ngit test contributor".into(),
            },
        )
        .await?;

    // Two-commit feature branch — `--defaults` will auto-select these as the
    // ahead-of-main commits when no range is passed.
    check_ok(
        "git checkout -b feature",
        clone
            .git(["checkout", "-b", "feature"])
            .output()
            .await
            .context("failed to spawn git checkout -b feature")?,
    )?;
    seed_commit(&clone, "t3.md", "some content\n").await?;
    seed_commit(&clone, "t4.md", "some content\n").await?;

    let send = clone
        .ngit(["--defaults", "send", "--force-patch"])
        .output()
        .await
        .context("failed to spawn ngit --defaults send --force-patch")?;
    check_ok("ngit --defaults send --force-patch", send)?;

    // Verify exactly two `Kind::GitPatch` events landed on the grasp and
    // none of them is a cover letter. `--defaults` for a patch run means "no
    // cover letter" — the legacy assertion shape, now read off the real
    // grasp.
    let author = read_clone_pubkey(&clone).await?;
    let events = harness
        .grasp("repo")
        .events(Filter::new().author(author).kind(Kind::GitPatch))
        .await?;
    assert_eq!(
        events.len(),
        2,
        "expected exactly 2 patch events authored by contributor on grasp; got {}",
        events.len(),
    );
    for ev in &events {
        assert!(
            !is_cover_letter(ev),
            "no patch event should carry `t cover-letter` under --defaults; saw event {} \
             with tags {:?}",
            ev.id,
            ev.tags,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// commits-behind-main: requires --force
// ---------------------------------------------------------------------------

/// Setup: maintainer publishes, then advances `origin/main` by one extra
/// commit and creates a 2-commit feature branch off the pre-advance commit.
///
/// The publisher (not a fresh clone) is used because `ngit send`'s
/// `get_main_or_master_branch` (`src/lib/git/mod.rs:170-180`) reads
/// `origin/main` if present, falling back to local `main` only when the
/// remote-tracking ref is missing. After `publish_repo`, every clone has an
/// `origin/main` pointing at the initial seed commit; advancing only the
/// local `main` branch would leave `origin/main` stale and ngit would not
/// see feature as "behind". Pushing the extra commit to origin keeps the
/// remote tracking ref in lock-step with what we expect ngit to read.
async fn setup_feature_behind_main(harness: &Harness) -> Result<Repo> {
    let (publisher, _published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    // Branch *before* the extra main commit lands so feature roots on the
    // pre-advance tip — otherwise feature would inherit the t5 commit as a
    // proper ancestor and not be "behind" relative to main.
    check_ok(
        "git checkout -b feature",
        publisher
            .git(["checkout", "-b", "feature"])
            .output()
            .await
            .context("failed to spawn git checkout -b feature")?,
    )?;
    seed_commit(&publisher, "t3.md", "some content\n").await?;
    seed_commit(&publisher, "t4.md", "some content\n").await?;

    // Advance origin/main by one commit so ngit's behind check sees feature
    // as 1 behind. Pushing — rather than just committing locally — is
    // load-bearing for the reason explained on the helper's doc comment.
    check_ok(
        "git checkout main",
        publisher
            .git(["checkout", "main"])
            .output()
            .await
            .context("failed to spawn git checkout main")?,
    )?;
    seed_commit(&publisher, "t5.md", "some content\n").await?;
    check_ok(
        "git push origin main (advance origin)",
        publisher
            .git(["push", "origin", "main"])
            .output()
            .await
            .context("failed to spawn git push origin main")?,
    )?;

    check_ok(
        "git checkout feature",
        publisher
            .git(["checkout", "feature"])
            .output()
            .await
            .context("failed to spawn git checkout feature (after advancing origin/main)")?,
    )?;

    Ok(publisher)
}

#[tokio::test]
async fn send_when_behind_main_errors_without_force() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let clone = setup_feature_behind_main(&harness).await?;

    // `--force-patch` pins the kind (per migration rules) but does NOT
    // bypass the behind check — that's the global `--force` flag. The error
    // we expect is from `check_commits_are_suitable_for_proposal:516-522`.
    let out = clone
        .ngit(["send", "HEAD~2", "--force-patch", "--no-cover-letter"])
        .output()
        .await
        .context("failed to spawn ngit send")?;
    if out.status.success() {
        bail!(
            "ngit send unexpectedly succeeded with feature behind main\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .to_ascii_lowercase();
    assert!(
        combined.contains("behind"),
        "expected behind-main error message; got:\n{combined}",
    );
    // The recommendation half — the bit that distinguishes a useful CLI
    // error from a bare bail. If this regresses (e.g. someone removes the
    // `use --force` hint) the test catches it.
    assert!(
        combined.contains("--force"),
        "expected error to recommend --force; got:\n{combined}",
    );
    Ok(())
}

#[tokio::test]
async fn send_when_behind_main_succeeds_with_force() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let clone = setup_feature_behind_main(&harness).await?;

    let send = clone
        .ngit([
            "send",
            "HEAD~2",
            "--force-patch",
            "--no-cover-letter",
            "--force",
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force")?;
    check_ok("ngit send --force-patch --no-cover-letter --force", send)?;

    // Sanity-check the events actually landed — without this, a regression
    // that turned `--force` into a no-op (or made send fail silently) would
    // pass the "command succeeded" exit-code check above. The grasp REQ is
    // the authoritative answer.
    let author = read_clone_pubkey(&clone).await?;
    let events = harness
        .grasp("repo")
        .events(Filter::new().author(author).kind(Kind::GitPatch))
        .await?;
    assert_eq!(
        events.len(),
        2,
        "expected 2 patch events on grasp after --force send; got {}",
        events.len(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `echo > <name> && git add && git commit -m "add <name>"`.
async fn seed_commit(repo: &Repo, file_name: &str, content: &str) -> Result<()> {
    std::fs::write(repo.dir().join(file_name), content)
        .with_context(|| format!("failed to write {file_name} into {}", repo.dir().display()))?;
    check_ok(
        "git add",
        repo.git(["add", file_name])
            .output()
            .await
            .with_context(|| format!("failed to spawn git add {file_name}"))?,
    )?;
    check_ok(
        "git commit",
        repo.git(["commit", "-m", &format!("add {file_name}"), "--no-gpg-sign"])
            .output()
            .await
            .with_context(|| format!("failed to spawn git commit for {file_name}"))?,
    )?;
    Ok(())
}

/// Seed a single benign commit on the current branch (assumed `main`).
async fn seed_main_commit(repo: &Repo) -> Result<()> {
    seed_commit(repo, "README.md", "hello\n").await
}

/// Pubkey of the identity logged in to `clone` — i.e. the value
/// `clone_published_repo(_, AsContributor { .. })` wrote to local
/// `nostr.nsec`. Inlined here rather than imported from `test_harness` so
/// the harness's public surface stays small (the same helper exists
/// privately in `scenarios.rs`).
async fn read_clone_pubkey(clone: &Repo) -> Result<PublicKey> {
    let nsec = clone
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from clone — was clone_published_repo called with a login?")?;
    let keys = Keys::parse(&nsec)
        .context("nostr.nsec in clone's local config is not a valid bech32 nsec")?;
    Ok(keys.public_key())
}

/// `true` when `event` carries `["t", "cover-letter"]`.
fn is_cover_letter(event: &Event) -> bool {
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("t")
            && s.get(1).map(String::as_str) == Some("cover-letter")
    })
}

/// Bail with a captured-output error message when a child process exits
/// non-zero.
fn check_ok(label: &str, out: std::process::Output) -> Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }
}
