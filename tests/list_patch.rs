//! Patch-kind sibling of `tests/list_pr.rs` — same regression assertions,
//! same legacy origin
//! (`tests/legacy/git_remote_nostr/list.rs::when_there_are_open_proposals`),
//! but driven through [`Harness::publish_three_open_patch_proposals`] so
//! the proposal events are `Kind::GitPatch` with a cover-letter root.
//!
//! `src/bin/git_remote_nostr/list.rs:273-291` is the branch under test:
//! when no `KIND_PULL_REQUEST` / `KIND_PULL_REQUEST_UPDATE` event is
//! present on the proposal's events, `list.rs` falls through to
//! `make_commits_for_proposal`, which applies each patch in the series
//! on top of the proposal's base commit to rebuild the tip. The
//! advertised ref shape is identical to the PR-kind variant (see
//! `tests/list_pr.rs`'s module-level doc) — only the construction
//! path differs.
//!
//! The legacy test was patch-kind under the hood (the legacy
//! `cli_tester_create_proposals` produced patch series); this file
//! preserves that regression. The PR-kind sibling (`tests/list_pr.rs`)
//! is the new default; both must hold for the migration to be safe to
//! land.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use nostr_sdk::EventId;
use test_harness::{
    CloneLogin, Harness, PublishRepoOpts, PublishedPatchSeries, PublishedRepo, Repo,
};

async fn setup() -> Result<(Harness, PublishedRepo, [PublishedPatchSeries; 3])> {
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
            display_name: Some("list-patch maintainer".into()),
            identifier: Some("list-patch-repo".into()),
            ..Default::default()
        })
        .await?;

    let series = harness
        .publish_three_open_patch_proposals(&published)
        .await?;
    Ok((harness, published, series))
}

/// `pr/<branch>(<8-hex>)` long-form for a patch series. The shorthand
/// hex is the first 8 chars of the *root* event id — the cover letter
/// when one exists, else the first patch. Mirrors
/// `CoverLetter::get_branch_name_with_pr_prefix_and_shorthand_id` in
/// `src/lib/git_events.rs:805-816`.
fn expected_long_branch(series: &PublishedPatchSeries) -> Result<String> {
    let root_id = root_event_id(series)?;
    let hex = root_id.to_hex();
    Ok(format!("pr/{}({})", series.branch_name, &hex[..8]))
}

/// Root event id of a patch series — cover letter when present, otherwise
/// the first per-commit patch. Tests in this file always use cover
/// letters (the default for `publish_three_open_patch_proposals`); the
/// fall-through is here for symmetry with `list.rs`'s own root-finding
/// logic so any future no-cover-letter sibling tests can reuse this
/// helper.
fn root_event_id(series: &PublishedPatchSeries) -> Result<EventId> {
    if let Some(cl) = &series.cover_letter_event {
        return Ok(cl.id);
    }
    series
        .patch_events
        .first()
        .map(|e| e.id)
        .context("patch series has no events — programmer error")
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

/// Patch-kind counterpart of
/// `tests/list_pr.rs::open_pr_proposals_are_listed_under_pr_namespaces`.
/// Same three-ref-form-per-proposal assertion shape; the construction
/// difference is hidden inside the scenario builder.
#[tokio::test]
async fn open_patch_proposals_are_listed_under_pr_namespaces() -> Result<()> {
    let (harness, published, series) = setup().await?;

    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    let ls = ls_remote(&test_repo, "origin").await?;

    for (idx, s) in series.iter().enumerate() {
        let long = expected_long_branch(s)?;
        let root_hex = root_event_id(s)?.to_hex();
        let expected: HashMap<String, &str> = [
            (format!("refs/heads/{long}"), s.tip.as_str()),
            (format!("refs/{long}"), s.tip.as_str()),
            (format!("refs/pr/{root_hex}/head"), s.tip.as_str()),
        ]
        .into_iter()
        .collect();

        for (ref_name, want_oid) in expected {
            let got = ls.refs.get(&ref_name).cloned();
            assert_eq!(
                got.as_deref(),
                Some(want_oid),
                "patch-series #{idx} ({:?}): expected {ref_name} → {want_oid}, got {got:?}\n\
                 full ls-remote refs: {:#?}",
                s.branch_name,
                ls.refs,
            );
        }
    }

    assert_eq!(
        ls.refs.get("refs/heads/main").map(String::as_str),
        Some(published.initial_oid.as_str()),
        "main should still be listed alongside the patch-series PR namespaces",
    );

    Ok(())
}
