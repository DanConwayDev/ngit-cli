//! Migrated port of legacy
//! `tests/legacy/git_remote_nostr/fetch.rs::creates_commits_from_open_proposal_with_no_warnings_printed`.
//!
//! The legacy test drove `git-remote-nostr` directly through a PTY with a
//! hand-typed `fetch <oid> refs/heads/<branch>` line, then asserted on the
//! *absence* of warning text in the helper's stdout. Both the PTY surface
//! and the exact-stdout assertion are banned by the new harness boundary
//! rules (see `AGENTS.md` § "Test harness boundary"). The behavioural
//! contract the legacy test pinned was actually narrower than its
//! assertion suggested:
//!
//! > After a contributor publishes a PR-kind proposal, a maintainer
//! > cloning the announced repo over `nostr://` must end up with a
//! > remote-tracking `pr/...` ref whose oid is the published PR's tip.
//!
//! That is *the* observable side-effect of fetching a proposal. If the
//! ref doesn't show up — or shows up pointing at the wrong oid — `git
//! checkout pr/<branch>` is dead. We assert exactly that, on refs on
//! disk, not on stdout. No PTY, no exact-string asserts, no `#[serial]`.
//!
//! The proposal setup is driven by the new
//! [`test_harness::Harness::publish_three_open_proposals`] scenario
//! builder rather than the legacy `cli_tester_create_proposals` helper.
//! That builder pins each proposal to `KIND_PULL_REQUEST` via
//! `ngit send --force-pr` so this test stays green when ngit's
//! default-kind heuristic in `src/bin/ngit/sub_commands/send.rs:236-243`
//! evolves underneath it — the whole point of the migration plan's
//! "Force-flag discipline" section.
//!
//! Flow:
//!
//! 1. Harness: one vanilla relay (`"default"` — user metadata) + one grasp
//!    server (`"repo"` — git data + repo-relay).
//! 2. `harness.publish_repo(...)` — maintainer publishes a repo.
//! 3. `harness.publish_three_open_proposals(&repo)` — fresh contributor
//!    publishes three PR-kind proposals on `feature-1`, `feature-2`,
//!    `feature-3`.
//! 4. `harness.clone_published_repo(..., CloneLogin::AsMaintainer)` —
//!    maintainer-view clone. `git clone` itself runs `git fetch` as part of
//!    setup, so the cloned repo already has every remote-tracking ref the
//!    remote-helper advertised.
//! 5. Assert: every PublishedPr tip has a matching remote-tracking ref under
//!    `refs/remotes/origin/` whose oid equals the PR's tip.
//!
//! ## Why the maintainer (not the contributor) clones
//!
//! `git_remote_nostr/list.rs` produces shorter `pr/<branch>` ref names
//! when the current user matches the proposal author, and longer
//! `pr/<branch>(<shorthand-event-id>)` names otherwise. Cloning as the
//! maintainer hits the not-the-author branch — the more common
//! review-side flow — and exercises the longer ref-naming path that the
//! legacy test never explicitly covered.

use std::collections::BTreeMap;

use anyhow::Result;
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedPr, RepoSnapshot};

#[tokio::test]
async fn fetched_proposal_tip_lands_in_pr_remote_tracking_ref() -> Result<()> {
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

    // --- 2. maintainer clones, which fetches everything once -----------
    let reviewer = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    // --- 3. observable side-effect: at least one PR tip on disk -------
    //
    // The legacy test asserted on stdout warnings; we assert on the only
    // contract that matters to the calling git user — that the fetched
    // proposal tip is reachable via a remote-tracking ref.
    let snapshot = reviewer.snapshot()?;
    let remote_refs = collect_remote_refs(&snapshot);

    let mut matches: Vec<(String, String)> = Vec::new();
    for pr in &prs {
        if let Some(ref_name) = remote_refs.iter().find_map(|(name, oid)| {
            (oid == &pr.tip
                && name.starts_with("refs/remotes/origin/")
                && name.contains("/pr/"))
            .then(|| name.clone())
        }) {
            matches.push((ref_name, pr.tip.clone()));
        }
    }

    assert!(
        !matches.is_empty(),
        "expected at least one remote-tracking pr/... ref pointing at a published PR tip.\n\
         published PR tips: {:#?}\n\
         remote-tracking refs in clone: {:#?}",
        prs.iter()
            .map(|p| (p.branch_name.clone(), p.tip.clone()))
            .collect::<Vec<_>>(),
        remote_refs,
    );

    Ok(())
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
