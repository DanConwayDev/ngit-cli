//! Migrated regression coverage for
//! `git-remote-nostr list`'s **open-proposal listing** code path —
//! specifically the PR-kind branch in
//! `src/bin/git_remote_nostr/list.rs:247-273` (read tip from the
//! KIND_PULL_REQUEST event's `c` tag, fetch that one OID from a git
//! server, advertise only when the OID is locally resolvable).
//!
//! Patch-kind sibling lives in `tests/list_patch.rs`; the
//! `get_open_and_draft_proposals_state` function in `list.rs` branches
//! sharply between the two kinds at line 247, so a single test file
//! couldn't cover both without `if branch { ... } else { ... }`
//! duplication. Same split rationale as `tests/pr_checkout.rs` /
//! `tests/pr_checkout_patch.rs`.
//!
//! Replaces legacy
//! `tests/legacy/git_remote_nostr/list.
//! rs::when_there_are_open_proposals::open_proposal_listed_in_prs_namespace`.
//! The legacy version ran `cli_tester_create_proposals` which produced
//! **patch-kind** proposals; the new harness uses
//! [`Harness::publish_three_open_proposals`] which produces PR-kind
//! proposals (and the migration plan's "PR-kind by default, patch
//! exception" disposition). Patch-kind regression for the same
//! assertions lives in `list_patch.rs`.
//!
//! ## What `list` advertises for an open proposal
//!
//! Three ref forms per open proposal (see `list.rs:247-296` + `:300-336`):
//!
//! - `refs/heads/pr/<branch>(<8-char-shorthand>)` — the "checkout this PR
//!   locally" form. `(<shorthand>)` is the first 8 hex chars of the proposal
//!   root event id; only present when the listing repo isn't logged in as the
//!   proposal author (our `CloneLogin::None` clone is always in this state).
//! - `refs/pr/<branch>(<8-char-shorthand>)` — same name, different namespace;
//!   the "scratch / fetch raw" form.
//! - `refs/pr/<event-id-hex>/head` — canonical "pinned by event id" form.
//!
//! All three resolve to the PR's `tip` (last commit in the series).

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedPr, PublishedRepo, Repo};

async fn setup() -> Result<(Harness, PublishedRepo, [PublishedPr; 3])> {
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
            display_name: Some("list-pr maintainer".into()),
            identifier: Some("list-pr-repo".into()),
            ..Default::default()
        })
        .await?;

    let prs = harness.publish_three_open_proposals(&published).await?;
    Ok((harness, published, prs))
}

/// `refs/heads/pr/<branch>(<8-hex>)` long-form — same construction as
/// `tests/pr_checkout.rs::expected_branch_name`, but here we need the bare
/// branch name (without the `refs/heads/` prefix) because we're matching
/// against the parsed `git ls-remote` ref→oid map directly.
fn expected_long_branch(pr: &PublishedPr) -> String {
    let hex = pr.event_id.to_hex();
    format!("pr/{}({})", pr.branch_name, &hex[..8])
}

#[derive(Debug)]
struct LsRemoteOutput {
    refs: BTreeMap<String, String>,
}

async fn ls_remote(repo: &Repo, remote: &str) -> Result<LsRemoteOutput> {
    let out = repo
        .git(["ls-remote", remote])
        .output()
        .await
        .with_context(|| format!("spawn git ls-remote {remote}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git ls-remote {remote} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).context("ls-remote stdout not utf-8")?;
    let mut refs = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("ref: ") {
            continue;
        }
        let (oid, name) = line
            .split_once('\t')
            .with_context(|| format!("malformed ls-remote line: {line:?}"))?;
        refs.insert(name.to_string(), oid.to_string());
    }
    Ok(LsRemoteOutput { refs })
}

/// Folds legacy
/// `when_there_are_open_proposals::open_proposal_listed_in_prs_namespace`.
///
/// The legacy version round-tripped `cli_tester_create_proposals` (three
/// patch-kind proposals) and then asserted the union of:
/// - main + example-branch from the state event
/// - per-proposal: `refs/heads/<long-branch>` + `refs/<long-branch>` +
///   `refs/pr/<event-id>/head`
/// equalled the entire ls-remote output (a `HashSet<String>` equality).
/// We collapse the same per-proposal expectations into one assertion
/// loop, and assert ⊆-style (every expected ref is present with the
/// right oid) — strict equality breaks under the new harness because
/// `git ls-remote` also advertises a few internal refs (e.g.
/// `HEAD` itself, the `^{}` peeled tag refs if any) that the legacy
/// PTY-driven raw `list` output didn't include.
#[tokio::test]
async fn open_pr_proposals_are_listed_under_pr_namespaces() -> Result<()> {
    let (harness, published, prs) = setup().await?;

    // CloneLogin::None: no `nostr.npub` set, so `list.rs:236` always
    // takes the long-form `pr/<branch>(<shorthand>)` ref-name path —
    // matching the legacy test_repo shape.
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    let ls = ls_remote(&test_repo, "origin").await?;

    // For each PR, every advertised ref must resolve to pr.tip.
    for (idx, pr) in prs.iter().enumerate() {
        let long = expected_long_branch(pr);
        let event_id_hex = pr.event_id.to_hex();
        let expected: HashMap<String, &str> = [
            (format!("refs/heads/{long}"), pr.tip.as_str()),
            (format!("refs/{long}"), pr.tip.as_str()),
            (format!("refs/pr/{event_id_hex}/head"), pr.tip.as_str()),
        ]
        .into_iter()
        .collect();

        for (ref_name, want_oid) in expected {
            let got = ls.refs.get(&ref_name).cloned();
            assert_eq!(
                got.as_deref(),
                Some(want_oid),
                "PR #{idx} ({:?}): expected {ref_name} → {want_oid}, got {got:?}\n\
                 full ls-remote refs: {:#?}",
                pr.branch_name,
                ls.refs,
            );
        }
    }

    // main was published with `publish_repo`'s seed; the announcement's
    // state event covers it. Sanity check that the proposal refs aren't
    // displacing the regular refs.
    assert_eq!(
        ls.refs.get("refs/heads/main").map(String::as_str),
        Some(published.initial_oid.as_str()),
        "main should still be listed alongside the PR namespaces",
    );

    Ok(())
}
