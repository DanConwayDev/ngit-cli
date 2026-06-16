//! End-to-end coverage of `ngit send` when the contributor is **checked out on
//! the default branch** (`main`) rather than a feature branch.
//!
//! ## Why this case matters
//!
//! `ngit send` derives the proposal's `merge-base` (fork point) from
//! `git_repo.get_commit_parent(first_commit)` — the parent of the first
//! selected commit (`send.rs`). When the contributor is on the default branch
//! and selects `HEAD~1` (one commit), the proposal commit is literally a commit
//! that lives on `main`, and its parent is the previous `main` tip. The
//! merge-base must therefore equal that previous tip.
//!
//! This is the companion to the multi-remote stale-origin fix on the
//! `git-remote-nostr` push path: that fix changed how the *push path* scopes
//! the proposal and fork point, and it must not perturb the separate `ngit
//! send` path, which already does the right thing for a from-default-branch
//! send. This test locks that behaviour in.
//!
//! ## Arrangement
//!
//! 1. Harness: one relay (`"default"`) + one GRASP server (`"repo"`).
//! 2. Maintainer publishes the repo; `published.initial_oid` is the baseline.
//! 3. Fresh contributor clones (stays on `main`).
//! 4. Contributor commits one file (`feature.md`) directly on `main`. The
//!    parent of this commit is `published.initial_oid` — the expected
//!    merge-base.
//! 5. Contributor runs `ngit send HEAD~1 --force-pr` while still on `main`.
//! 6. [`capture_snapshot`] reads the resulting PR event.
//!
//! ## Coverage
//!
//! 1. **one_pr_event** — exactly one KIND_PULL_REQUEST is published.
//! 2. **merge_base_is_parent_of_selected_commit** — the `merge-base` tag equals
//!    `published.initial_oid` (the parent of the on-main proposal commit).
//! 3. **c_tag_is_proposal_tip** — the `c` tag equals the proposal commit OID.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, tag_value};
use tokio::sync::OnceCell;

const IDENTIFIER: &str = "send-from-default-branch-test-repo";

struct Snapshot {
    pr_event: Event,
    pr_count: usize,
    /// Parent of the on-main proposal commit (= clone-time main tip). The
    /// merge-base tag must equal this (assertion 2).
    expected_merge_base_oid: String,
    /// The proposal commit OID. The `c` tag must equal this (assertion 3).
    proposal_tip_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("send_pr_from_default_branch fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

async fn capture_snapshot() -> Result<Snapshot> {
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
            display_name: Some("send-from-main maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "send-from-main contributor".into(),
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

    // Commit directly on main (no feature branch checkout).
    std::fs::write(contributor.dir().join("feature.md"), "on-main change\n")
        .context("failed to write feature.md")?;
    contributor
        .git_ok(["add", "feature.md"], "git add feature.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add feature.md", "--no-gpg-sign"],
            "git commit feature.md",
        )
        .await?;

    let proposal_tip_oid = contributor.rev_parse("HEAD").await?;
    let expected_merge_base_oid = contributor.rev_parse("HEAD~1").await?;

    // Precondition: parent of the on-main commit is the clone-time main tip.
    if expected_merge_base_oid != published.initial_oid {
        bail!(
            "setup invariant violated: HEAD~1 ({expected_merge_base_oid}) != initial_oid ({})",
            published.initial_oid
        );
    }

    // `ngit send HEAD~1 --force-pr` from the default branch.
    let out = contributor
        .ngit([
            "send",
            "HEAD~1",
            "--force-pr",
            "--title",
            "on-main proposal",
            "--description",
            "a change sent while checked out on main",
        ])
        .output()
        .await
        .context("failed to spawn ngit send HEAD~1 --force-pr from main")?;
    if !out.status.success() {
        bail!(
            "ngit send from main exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    let pr_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count = pr_events.len();
    let pr_event = pr_events.into_iter().next().context(
        "no KIND_PULL_REQUEST authored by contributor found on GRASP after `ngit send` from main",
    )?;

    Ok(Snapshot {
        pr_event,
        pr_count,
        expected_merge_base_oid,
        proposal_tip_oid,
    })
}

/// Assertion 1: exactly one KIND_PULL_REQUEST is published.
#[rstest]
#[tokio::test]
async fn one_pr_event(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 1,
        "expected exactly one KIND_PULL_REQUEST on GRASP after `ngit send` from main; got {}",
        s.pr_count,
    );
    Ok(())
}

/// Assertion 2: the `merge-base` tag equals the parent of the on-main proposal
/// commit (the clone-time main tip). `ngit send` uses
/// `get_commit_parent(first_commit)` for the fork point, so sending from the
/// default branch records the parent of the selected commit — not the commit
/// itself, and not some remote's default-branch tip.
#[rstest]
#[tokio::test]
async fn merge_base_is_parent_of_selected_commit(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "merge-base").as_deref(),
        Some(s.expected_merge_base_oid.as_str()),
        "merge-base tag should equal the parent of the on-main proposal commit; got {:?}, want {:?}",
        tag_value(&s.pr_event, "merge-base"),
        s.expected_merge_base_oid,
    );
    Ok(())
}

/// Assertion 3: the `c` tag equals the on-main proposal commit OID.
#[rstest]
#[tokio::test]
async fn c_tag_is_proposal_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "c").as_deref(),
        Some(s.proposal_tip_oid.as_str()),
        "c tag should equal the on-main proposal commit OID; got {:?}, want {:?}",
        tag_value(&s.pr_event, "c"),
        s.proposal_tip_oid,
    );
    Ok(())
}
