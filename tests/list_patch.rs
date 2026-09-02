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
use nostr::{
    event::FinalizeEvent,
    prelude::{Event, EventBuilder, EventId, Keys, Kind, Tag},
};
use nostr_sdk::prelude::Client;
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

async fn publish_to_repo_grasp(harness: &Harness, event: &Event) -> Result<()> {
    let relay_url = harness.grasp("repo").relay_url();
    let client = Client::default();
    client
        .add_relay(&relay_url)
        .await
        .with_context(|| format!("failed to add repository relay {relay_url}"))?;
    client.connect().await;
    let output = client
        .send_event(event)
        .to([relay_url.as_str()])
        .await
        .with_context(|| format!("failed to publish malformed patch to {relay_url}"))?;
    client.disconnect().await;
    anyhow::ensure!(
        output.failed.is_empty(),
        "repository relay rejected malformed patch {}: {:?}",
        event.id,
        output.failed,
    );
    Ok(())
}

/// Patch-kind counterpart of
/// `tests/list_pr.
/// rs::enabling_auto_pr_branches_lists_open_prs_under_pr_namespaces`.
/// Same three-ref-form-per-proposal assertion shape; the construction
/// difference is hidden inside the scenario builder.
#[tokio::test]
async fn enabling_auto_pr_branches_lists_open_patch_proposals() -> Result<()> {
    let (harness, published, series) = setup().await?;

    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    git_ok(
        &test_repo,
        ["config", "--local", "nostr.auto-pr-branches", "true"],
        "enable automatic PR branches",
    )
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

// This exercises v3's explicit compatibility opt-in. A default fresh clone has
// no selected `pr/` branch and returns before parsing proposal patches.
#[tokio::test]
async fn malformed_patch_events_do_not_break_passive_ref_listing() -> Result<()> {
    let (harness, published, series) = setup().await?;
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    let template = series[0]
        .cover_letter_event
        .as_ref()
        .context("patch-series fixture should include a cover letter")?;

    let mut malformed_description_tags = template.tags.clone().to_vec();
    malformed_description_tags
        .retain(|tag| tag.as_slice().first().map(String::as_str) != Some("branch-name"));
    malformed_description_tags.push(Tag::custom(
        "branch-name",
        vec!["malformed-description".to_string()],
    ));
    malformed_description_tags.push(Tag::parse(["description"])?);
    let malformed_description = EventBuilder::new(Kind::GitPatch, template.content.clone())
        .tags(malformed_description_tags)
        .finalize(&Keys::generate())?;
    publish_to_repo_grasp(&harness, &malformed_description).await?;

    let mut malformed_envelope_tags: Vec<Tag> = template
        .tags
        .iter()
        .filter(|tag| {
            let values = tag.as_slice();
            values.first().map(String::as_str) != Some("branch-name")
                && !(values.first().map(String::as_str) == Some("t")
                    && values.get(1).map(String::as_str) == Some("cover-letter"))
        })
        .cloned()
        .collect();
    malformed_envelope_tags.extend([
        Tag::custom("branch-name", vec!["malformed-envelope".to_string()]),
        Tag::custom("description", vec!["malformed envelope".to_string()]),
        Tag::custom("parent-commit", vec![published.initial_oid.clone()]),
    ]);
    let malformed_envelope = EventBuilder::new(
        Kind::GitPatch,
        format!(
            "From {}💣 Mon Sep 17 00:00:00 2001\nSubject: [PATCH] malformed envelope\n",
            "a".repeat(39),
        ),
    )
    .tags(malformed_envelope_tags)
    .finalize(&Keys::generate())?;
    publish_to_repo_grasp(&harness, &malformed_envelope).await?;

    git_ok(
        &test_repo,
        ["config", "--local", "nostr.auto-pr-branches", "true"],
        "enable automatic PR branches",
    )
    .await?;
    let ls = ls_remote(&test_repo, "origin").await?;

    assert_eq!(
        ls.refs.get("refs/heads/main").map(String::as_str),
        Some(published.initial_oid.as_str()),
        "malformed proposals must not suppress ordinary repository refs",
    );
    for valid in &series {
        assert_eq!(
            ls.refs
                .get(&format!("refs/heads/{}", expected_long_branch(valid)?))
                .map(String::as_str),
            Some(valid.tip.as_str()),
            "malformed proposals must not suppress valid proposal refs",
        );
    }
    assert!(
        ls.refs.keys().all(|name| {
            !name.contains("malformed-description") && !name.contains("malformed-envelope")
        }),
        "malformed proposals must not be advertised: {:#?}",
        ls.refs,
    );

    Ok(())
}

#[tokio::test]
async fn default_auto_pr_branches_reconstructs_only_checked_out_patch_series() -> Result<()> {
    let (harness, published, series) = setup().await?;
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    let before_checkout = ls_remote(&test_repo, "origin").await?;
    assert!(
        before_checkout
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/heads/pr/") && !name.starts_with("refs/pr/")),
        "no patch-series refs should be advertised before checkout: {:#?}",
        before_checkout.refs,
    );

    let selected = &series[0];
    let checkout = test_repo
        .ngit(["pr", "checkout", &root_event_id(selected)?.to_hex()])
        .output()
        .await
        .context("failed to spawn ngit pr checkout")?;
    anyhow::ensure!(
        checkout.status.success(),
        "ngit pr checkout exited {:?}\nstdout: {}\nstderr: {}",
        checkout.status,
        String::from_utf8_lossy(&checkout.stdout),
        String::from_utf8_lossy(&checkout.stderr),
    );

    let branch = expected_long_branch(selected)?;
    let after_checkout = ls_remote(&test_repo, "origin").await?;
    assert_eq!(
        after_checkout
            .refs
            .get(&format!("refs/heads/{branch}"))
            .map(String::as_str),
        Some(selected.tip.as_str()),
        "the checked-out patch series should be reconstructed and advertised",
    );
    assert_eq!(
        after_checkout
            .refs
            .keys()
            .filter(|name| name.starts_with("refs/heads/pr/"))
            .count(),
        1,
        "only the checked-out patch series should appear as a PR branch",
    );

    Ok(())
}
