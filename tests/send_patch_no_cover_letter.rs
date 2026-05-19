//! Patch-kind regression coverage for `ngit send --no-cover-letter` — successor
//! to the three legacy keepers inside `tests/legacy/ngit_send.
//! rs::when_no_cover_letter_flag_set_with_range_of_head_2_sends_2_patches_without_cover_letter`
//! (`first_patch_with_root_t_tag`, `root_patch_tags_branch_name`,
//! `second_patch_lists_first_as_root`).
//!
//! ## Shape
//!
//! - 1 vanilla relay (`"default"`) + 1 GRASP (`"repo"`); single maintainer
//!   (these assertions do not touch per-maintainer `a` / `p` tags, so the extra
//!   co-maintainer machinery in [`tests/send_patch.rs`](send_patch.rs) is
//!   unnecessary noise here).
//! - Maintainer publishes a repo via [`Harness::publish_repo`].
//! - A fresh contributor clone published by [`Harness::publish_patch_series`]
//!   commits two files and runs `ngit send HEAD~2 --force-patch
//!   --no-cover-letter`. The resulting patch series has **no cover letter**, so
//!   the *first patch* becomes the proposal root and the *second patch* threads
//!   off it via an `e <root>` tag.
//!
//! ## rstest discipline
//!
//! Each `#[case]` is a read-only assertion on the captured patch events.
//! No case mutates the repo or the relays; the fixture rebuilds the harness
//! per case (no `#[once]`).
//!
//! ## Dropped from legacy
//!
//! `cli_ouput::check_cli_output` — banned by harness rules
//! (`docs/architecture/test-harness.md` § "Anti-patterns"). The underlying
//! send-succeeds-and-publishes-2-patches contract is asserted by every other
//! case in this file (and by `publish_patch_series` itself, which fails the
//! scenario if the expected event count is off).

use anyhow::{Context, Result, anyhow};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, PublishPatchSeriesOpts, PublishRepoOpts};

const FIRST_COMMIT_FILE: &str = "t3.md";
const SECOND_COMMIT_FILE: &str = "t4.md";

/// Captured side-effects of one `ngit send --force-patch --no-cover-letter`
/// invocation. Populated once per fixture call; rstest cases read fields
/// and never mutate.
struct Snapshot {
    /// Patch event for the first commit (`t3.md`) — the root of the
    /// series in a no-cover-letter run.
    first_patch: Event,
    /// Patch event for the second commit (`t4.md`).
    second_patch: Event,
    /// Branch name the contributor committed on — matches the
    /// `branch-name` tag on the first (root) patch.
    branch_name: String,
}

#[fixture]
async fn snapshot() -> Snapshot {
    capture_snapshot()
        .await
        .expect("send_patch_no_cover_letter fixture: capture_snapshot failed")
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

    let (_publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    let series = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                commits: vec![
                    (FIRST_COMMIT_FILE.into(), "some content\n".into()),
                    (SECOND_COMMIT_FILE.into(), "some content\n".into()),
                ],
                cover_letter: None,
                ..Default::default()
            },
        )
        .await?;

    let first_commit_oid = series
        .commits
        .first()
        .cloned()
        .context("publish_patch_series returned no commits")?;
    let second_commit_oid = series
        .commits
        .get(1)
        .cloned()
        .context("publish_patch_series returned only one commit")?;

    let first_patch = find_patch_for_commit(&series.patch_events, &first_commit_oid)?;
    let second_patch = find_patch_for_commit(&series.patch_events, &second_commit_oid)?;

    Ok(Snapshot {
        first_patch,
        second_patch,
        branch_name: series.branch_name.clone(),
    })
}

/// Find the patch event whose `["commit", <oid>]` tag matches `oid`.
fn find_patch_for_commit(patches: &[Event], oid: &str) -> Result<Event> {
    patches
        .iter()
        .find(|e| tag_value(e, "commit").as_deref() == Some(oid))
        .cloned()
        .ok_or_else(|| anyhow!("no patch event found with commit tag {oid}"))
}

/// Value of the first tag whose first slot equals `key`.
fn tag_value(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

/// All values of every tag whose first slot equals `key`.
fn tag_values(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some(key) {
                s.get(1).cloned()
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Case {
    /// First patch carries `["t", "root"]`; second does not. The "t root"
    /// marker is the canonical signal that this event is the proposal
    /// root — in a no-cover-letter series the first patch fills that
    /// role, since there is no cover letter to carry the marker.
    FirstHasTRootSecondDoesnt,
    /// First (root) patch carries `["branch-name", <feature-branch>]`.
    /// Per `src/lib/git_events.rs:260-271` this tag is emitted only on
    /// the **root** of the series; per-commit descendants do not carry
    /// it. Useful for clients building `pr/<branch>` ref names.
    FirstHasBranchName,
    /// Second patch threads off the first via a 4-slot
    /// `["e", <first_patch_id>, _, "root"]` tag. Downstream clients
    /// follow this marker to reconstruct the series order.
    SecondHasRootETagToFirst,
}

#[rstest]
#[case::first_has_t_root_second_doesnt(Case::FirstHasTRootSecondDoesnt)]
#[case::first_has_branch_name(Case::FirstHasBranchName)]
#[case::second_has_root_e_tag_to_first(Case::SecondHasRootETagToFirst)]
#[tokio::test]
async fn no_cover_letter_tags(#[future] snapshot: Snapshot, #[case] case: Case) -> Result<()> {
    let s = snapshot.await;
    match case {
        Case::FirstHasTRootSecondDoesnt => {
            assert!(
                tag_values(&s.first_patch, "t").iter().any(|v| v == "root"),
                "first patch (proposal root in no-cover-letter mode) should carry `t root`; \
                 tags were {:?}",
                s.first_patch.tags,
            );
            assert!(
                !tag_values(&s.second_patch, "t").iter().any(|v| v == "root"),
                "second patch must not carry `t root` — that tag belongs to the series root only; \
                 tags were {:?}",
                s.second_patch.tags,
            );
        }
        Case::FirstHasBranchName => {
            assert_eq!(
                tag_value(&s.first_patch, "branch-name").as_deref(),
                Some(s.branch_name.as_str()),
                "first (root) patch should carry `branch-name <feature-branch>`",
            );
        }
        Case::SecondHasRootETagToFirst => {
            let root_e = s
                .second_patch
                .tags
                .iter()
                .find(|t| {
                    let v = t.as_slice();
                    v.first().map(String::as_str) == Some("e")
                        && v.len() == 4
                        && v.get(3).map(String::as_str) == Some("root")
                })
                .ok_or_else(|| {
                    anyhow!(
                        "second patch missing root-marker `e` tag (tags: {:?})",
                        s.second_patch.tags
                    )
                })?;
            assert_eq!(
                root_e.as_slice().get(1).map(String::as_str),
                Some(s.first_patch.id.to_hex().as_str()),
                "second patch's `e <root>` should point at the first patch",
            );
        }
    }
    Ok(())
}
