//! Default-fetch regression coverage for foreign proposals. A routine clone
//! must cache enough Nostr metadata for later `ngit pr checkout`, but it must
//! not create remote-tracking proposal branches or download their tip commits.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedPr, Repo, RepoSnapshot};

#[tokio::test]
async fn foreign_proposal_branches_and_tips_are_not_fetched_by_default() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    // --- 1. maintainer publishes a repo, contributor publishes 3 PRs ----
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("fetch-proposal maintainer".into()),
            identifier: Some("fetch-proposal-repo".into()),
            ..Default::default()
        })
        .await?;

    let prs: [PublishedPr; 3] = harness.publish_three_open_proposals(&published).await?;

    // --- 2. maintainer clones with the default selective behavior ------
    let reviewer = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    // --- 3. no proposal branches or proposal-only tip objects ----------
    let snapshot = reviewer.snapshot()?;
    let remote_refs = collect_remote_refs(&snapshot);
    assert!(
        remote_refs.keys().all(|name| !name.contains("/pr/")),
        "default clone unexpectedly created foreign PR tracking refs: {:#?}",
        remote_refs,
    );
    for pr in &prs {
        assert!(
            !commit_exists(&reviewer, &pr.tip).await?,
            "default clone unexpectedly downloaded proposal tip {} for {:?}",
            pr.tip,
            pr.branch_name,
        );
    }

    Ok(())
}

async fn commit_exists(repo: &Repo, oid: &str) -> Result<bool> {
    let object = format!("{oid}^{{commit}}");
    let out = repo
        .git(["cat-file", "-e", &object])
        .output()
        .await
        .with_context(|| format!("failed to inspect proposal commit {oid}"))?;
    Ok(out.status.success())
}

/// Filter a snapshot's refs down to `refs/remotes/...` entries. Returns a
/// `BTreeMap` so the diagnostic on assertion failure is sorted and
/// reproducible.
fn collect_remote_refs(snapshot: &RepoSnapshot) -> BTreeMap<String, String> {
    snapshot
        .refs
        .iter()
        .filter(|(name, _)| name.starts_with("refs/remotes/"))
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect()
}
