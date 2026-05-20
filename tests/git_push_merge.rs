//! Tests that pushing a merge commit to `refs/heads/main` causes the
//! `git-remote-nostr` push pipeline to publish a `Kind::GitStatusApplied`
//! (kind 1631) event tying the merged PR to the merge commit.
//!
//! The producing code lives in
//! `src/bin/git_remote_nostr/push.rs::get_merged_status_events` (entry at
//! `push.rs:361`); the event shape is built by `create_merge_status`
//! (`push.rs:1402-1501`). Only the merge-commit branch
//! (`MergedPRCommitType::MergeCommit`) is covered here — fast-forward and
//! apply-as-patches branches are deliberately left for follow-up tests:
//!
//! - **Fast-forward** (`merge_pr_with_fast_forward`): maintainer runs `git
//!   merge --ff-only` so `main` advances to `pr.tip` with no merge commit.
//!   `merged_patches` then carries one `PatchCommit` per ahead commit and
//!   `create_merge_status` falls through to the `merge-commit-id` tag with the
//!   **PR tip** as its value (not a merge-commit, but the same tag name — see
//!   `push.rs:1474-1484`).
//! - **Apply-as-commits** (`merge_pr_by_applying_patches`): maintainer
//!   re-creates the PR's changes with fresh commit IDs (e.g. `git cherry-pick`,
//!   or `ngit pr apply` if/when that lands). `applied` becomes true at
//!   `push.rs:1382-1387` and the tag name switches to `applied-as-commits`.
//!
//! The helpers in this file are factored so adding those is just another
//! `merge_pr_*` async fn plus a fresh `#[tokio::test]` that asserts on the
//! different tag-name / merge-oid-vs-pr-tip combination. The shared
//! [`Setup`], [`find_merge_status_event`], and [`tag_first_value`]
//! / [`event_root_e_tag`] primitives stay untouched.
//!
//! ## "As the maintainer" — repo choice
//!
//! `publish_repo` already returns the maintainer's local working tree with
//! `origin` pointing at the nostr:// URL, the nsec persisted in local git
//! config (so subsequent ngit invocations sign as the maintainer), and an
//! upstream wired by the post-init `nostr_push -u origin main`. That is
//! sufficient to drive a merge + push end-to-end, so the test uses it
//! directly rather than spinning up a fresh `clone_published_repo(...,
//! AsMaintainer)`. Either repo would hit the same `get_merged_status_events`
//! code path on push; the choice is purely setup-cost.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishPrOpts, PublishRepoOpts, PublishedPr, PublishedRepo, Repo};

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

struct Setup {
    /// Held only to keep the relay + grasp subprocess alive for the duration
    /// of the test. Used in assertions via `harness.grasp("repo").events(...)`.
    harness: Harness,
    /// Maintainer-published repo metadata — `published.maintainer_keys`
    /// is what signs the kind-1631 status event we assert on.
    published: PublishedRepo,
    /// The PR being merged.
    pr: PublishedPr,
    /// Maintainer's local working tree (the one `publish_repo` returns).
    /// Has `origin` configured, the maintainer nsec in local config, and
    /// `main` checked out with upstream tracking already set.
    maintainer_repo: Repo,
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

    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("merge-test maintainer".into()),
            identifier: Some("merge-test-repo".into()),
            ..Default::default()
        })
        .await?;

    // One PR is enough to exercise the merge-commit path. Branch name
    // pinned so tests reading the event's `branch-name` tag are stable.
    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: Some("feature".into()),
                commits: vec![
                    ("a.md".to_string(), "alpha\n".to_string()),
                    ("b.md".to_string(), "beta\n".to_string()),
                ],
                title: "merge me".into(),
                description: "please merge".into(),
                in_reply_to: vec![],
            },
        )
        .await?;

    Ok(Setup {
        harness,
        published,
        pr,
        maintainer_repo,
    })
}

// ---------------------------------------------------------------------------
// Refs / subprocess helpers
// ---------------------------------------------------------------------------

/// `pr/<branch>(<8-hex>)` — the form `git-remote-nostr/list.rs:235-244`
/// emits for a proposal whose author differs from the current user. The
/// maintainer always falls into this branch because the PR was authored
/// by the contributor `publish_pr` minted.
fn pr_long_branch(pr: &PublishedPr) -> String {
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

// ---------------------------------------------------------------------------
// Merge strategies
//
// Each strategy returns the local oid that `create_merge_status` will copy
// into the status event's `merge-commit-id` / `applied-as-commits` tag,
// plus a marker discriminating which tag-name the caller should assert on.
// New strategies (fast-forward, apply-as-patches) plug in here.
// ---------------------------------------------------------------------------

/// `git fetch origin` then `git merge --no-ff origin/<long-pr-branch>` from
/// `main`. Produces a true merge commit (two parents: prior main tip and
/// pr.tip). Returns the merge commit's oid.
///
/// Precondition: `repo` has `main` checked out and `origin` configured —
/// both true of `publish_repo`'s returned working tree and of any
/// `clone_published_repo` clone.
async fn merge_pr_with_merge_commit(repo: &Repo, pr: &PublishedPr) -> Result<String> {
    git_ok(repo, ["fetch", "origin"], "git fetch origin").await?;

    // Sanity: the remote helper advertises pr.tip under the long-form
    // branch — if this fails the rest of the test is meaningless and
    // the error here will be much clearer than a downstream
    // "merge-commit-id != merge_oid" mismatch.
    let remote_ref = format!("origin/{}", pr_long_branch(pr));
    let remote_tip = rev_parse(repo, &remote_ref).await?;
    anyhow::ensure!(
        remote_tip == pr.tip,
        "after `git fetch origin`, {remote_ref} resolved to {remote_tip}; \
         expected pr.tip {} — did list.rs stop advertising the long-form ref?",
        pr.tip,
    );

    // `--no-ff` is load-bearing: a fast-forward from a 0-commit main onto
    // a 2-commit PR would advance main to pr.tip with no merge commit,
    // which is the *fast-forward* path covered by a future sibling test —
    // not what this one is exercising.
    git_ok(
        repo,
        [
            "merge",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            &format!("Merge {}", pr_long_branch(pr)),
            &remote_ref,
        ],
        "git merge --no-ff",
    )
    .await?;

    let merge_oid = rev_parse(repo, "HEAD").await?;
    anyhow::ensure!(
        merge_oid != pr.tip,
        "expected --no-ff to produce a merge commit distinct from the PR \
         tip; HEAD is at pr.tip ({merge_oid}) — git silently fast-forwarded?",
    );
    Ok(merge_oid)
}

// ---------------------------------------------------------------------------
// Status-event lookup / tag accessors
// ---------------------------------------------------------------------------

/// Find the single `Kind::GitStatusApplied` event on the grasp's relay
/// signed by `signer_pubkey` whose root `e` tag points at `pr.event_id`.
/// Bails on zero or multiple matches — the push pipeline emits exactly one
/// status event per merged proposal per push.
async fn find_merge_status_event(
    harness: &Harness,
    pr: &PublishedPr,
    signer_pubkey: PublicKey,
) -> Result<Event> {
    let events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(signer_pubkey)
                .kind(Kind::GitStatusApplied),
        )
        .await?;
    let mut matches: Vec<Event> = events
        .into_iter()
        .filter(|e| event_root_e_tag(e) == Some(pr.event_id))
        .collect();
    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => anyhow::bail!(
            "no Kind::GitStatusApplied event from {signer_pubkey} found on grasp `repo` \
             whose root `e` tag matches pr.event_id={}",
            pr.event_id,
        ),
        _ => anyhow::bail!(
            "expected exactly 1 Kind::GitStatusApplied event from {signer_pubkey} for \
             pr.event_id={}; found {}",
            pr.event_id,
            matches.len(),
        ),
    }
}

/// EventId carried by the `e` tag with marker `root`. `create_merge_status`
/// emits exactly one such tag pointing at the merged proposal (see
/// `push.rs:1428-1434`); when a revision is involved a second `e/root` tag
/// is added for the revision (`push.rs:1447-1454`), so we don't insist on
/// uniqueness here — only that *some* root-marked `e` tag points at the
/// proposal id under test.
///
/// The matcher inspects every position in the tag for the literal `"root"`
/// rather than indexing into position 3, because the relay-url position
/// can be omitted / present depending on whether `repo_ref.relays` was
/// non-empty at sign time.
fn event_root_e_tag(event: &Event) -> Option<EventId> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) != Some("e") {
            return None;
        }
        if !s.iter().any(|v| v == "root") {
            return None;
        }
        s.get(1)
            .and_then(|hex| EventId::from_hex(hex.as_str()).ok())
    })
}

/// First value of the `["<key>", <value>, ...]` tag, if any. Used for
/// `alt`, `merge-commit-id`, `applied-as-commits`.
fn tag_first_value<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).map(String::as_str)
        } else {
            None
        }
    })
}

/// `true` when `event` carries `["r", <oid>]` for the given commit oid.
/// `push.rs:1486-1492` emits one such tag per merge commit, in addition
/// to the `["r", <repo-root-commit>]` advertisement at `push.rs:1471-1473`,
/// so the test checks for a specific value rather than a count.
fn has_r_tag(event: &Event, oid: &str) -> bool {
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("r") && s.get(1).map(String::as_str) == Some(oid)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Maintainer fetches the contributor's PR, merges it with `--no-ff`, and
/// pushes the resulting merge commit to `origin/main`. The push pipeline
/// must publish a single `Kind::GitStatusApplied` event tying the PR to
/// the merge commit.
#[tokio::test]
async fn merge_commit_publishes_status_event_referencing_proposal_and_commit() -> Result<()> {
    let Setup {
        harness,
        published,
        pr,
        maintainer_repo,
    } = setup().await?;

    let merge_oid = merge_pr_with_merge_commit(&maintainer_repo, &pr).await?;

    // Push must go via `nostr_push` so the auto-generated kind-30618
    // state event covering the new main tip doesn't collide on
    // `created_at` with the previous state event from `publish_repo`'s
    // post-init push — see `test_harness::clock` for the writeup.
    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after merge")?;

    let event =
        find_merge_status_event(&harness, &pr, published.maintainer_keys.public_key()).await?;

    // `merge-commit-id` carries the merge commit's oid — that's the
    // tag downstream tooling uses to look the commit up on the git
    // server. It must NOT be pr.tip: that would be the fast-forward
    // path's value and would silently misclassify the merge.
    let merge_tag = tag_first_value(&event, "merge-commit-id").with_context(|| {
        format!(
            "status event {} has no `merge-commit-id` tag — full event: {event:?}",
            event.id,
        )
    })?;
    assert_eq!(
        merge_tag, merge_oid,
        "merge-commit-id tag should carry the merge commit's oid",
    );
    assert_ne!(
        merge_tag, pr.tip,
        "merge-commit-id must differ from the PR tip — that's how downstream \
         distinguishes merge commits from fast-forwards",
    );

    // Canonical human-readable alt summary — `push.rs:1424-1427`.
    assert_eq!(
        tag_first_value(&event, "alt"),
        Some("git proposal merged / applied"),
        "alt tag should match the canonical merge / applied summary",
    );

    // The push pipeline emits one `["r", <merge-oid>]` per merge commit
    // in addition to the announcement's root-commit `r` tag — confirm
    // the merge oid specifically is listed.
    assert!(
        has_r_tag(&event, &merge_oid),
        "status event should carry an `r` tag with the merge commit oid {merge_oid}; \
         full tags: {:?}",
        event.tags,
    );

    // The merge-commit branch must not also emit an `applied-as-commits`
    // tag — that's the patch-application path's discriminator
    // (`push.rs:1474-1484`). Catches a regression that would merge the
    // two branches in `create_merge_status`.
    assert!(
        tag_first_value(&event, "applied-as-commits").is_none(),
        "merge-commit path should not emit an `applied-as-commits` tag",
    );

    Ok(())
}
