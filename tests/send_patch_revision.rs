//! Patch-kind regression coverage for `ngit send --in-reply-to <existing-root>`
//! producing a **proposal revision** — successor to the legacy keepers under
//! `tests/legacy/ngit_send.
//! rs::root_proposal_specified_using_in_reply_to_with_range_of_head_2_and_cover_letter_details_specified`
//! (`cover_letter_tags::{t_tag_root, t_tag_revision_root,
//! e_tag_in_reply_to_event_as_reply}` and
//! `patch_tags_cover_letter_event_as_root`).
//!
//! ## Shape
//!
//! - 1 vanilla relay (`"default"`) + 1 GRASP (`"repo"`); single maintainer
//!   (none of these assertions touch per-maintainer `a` / `p` tags).
//! - Maintainer publishes the repo via [`Harness::publish_repo`].
//! - **Original proposal**: a first contributor publishes a cover-lettered
//!   patch series (`a3.md`, `a4.md` on `feature-1`).
//! - **Revision**: a second contributor publishes another cover-lettered patch
//!   series (`b3.md`, `b4.md` on `feature-2`) with `--in-reply-to
//!   <original_cover_letter_id>`. This is the event we assert against.
//!
//! Two different contributor identities is incidental (each
//! [`Harness::publish_patch_series`] call mints a fresh contributor) and
//! does not affect the assertions: ngit resolves `--in-reply-to` by event id,
//! not by author.
//!
//! ## rstest discipline
//!
//! Each `#[case]` is a read-only assertion on the captured revision events.
//! No case mutates the repo or the relays; the fixture rebuilds the harness
//! per case.
//!
//! ## Dropped from legacy
//!
//! `cli_ouput::check_cli_output` — banned by harness rules
//! (`docs/architecture/test-harness.md` § "Anti-patterns"). The
//! revision-is-published contract is asserted by every other case in this
//! file.

use anyhow::{Context, Result, anyhow};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, PublishPatchSeriesOpts, PublishRepoOpts};

const ORIGINAL_TITLE: &str = "original proposal";
const ORIGINAL_DESCRIPTION: &str = "the proposal being revised";
const REVISION_TITLE: &str = "revised proposal";
const REVISION_DESCRIPTION: &str = "the revision";

/// Captured side-effects of one revision-publish (an `ngit send
/// --force-patch --title ... --description ... --in-reply-to <original>`
/// against an existing patch-series proposal).
struct Snapshot {
    /// Cover letter of the **revision** (the event under test). Its
    /// tags carry both the new-proposal markers (`t root`) and the
    /// revision-specific markers (`t revision-root`, `e <reply>` →
    /// original).
    revision_cover_letter: Event,
    /// One of the per-commit patches in the revision series (we pick
    /// the first by commit oid). Used to assert that per-commit patches
    /// thread off the revision's own cover letter via `e <root>`.
    revision_patch: Event,
    /// Cover letter id of the **original** proposal — the value the
    /// revision's `e <reply>` tag should point at.
    original_cover_letter_id: EventId,
}

#[fixture]
async fn snapshot() -> Snapshot {
    capture_snapshot()
        .await
        .expect("send_patch_revision fixture: capture_snapshot failed")
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

    // --- original proposal -------------------------------------------------
    //
    // Cover-lettered so the resulting `cover_letter_event` (kind 1617 with
    // `t cover-letter`) can serve as the `--in-reply-to` anchor for the
    // revision. Branch + commit names are arbitrary; we pick `feature-1` /
    // `a*.md` to leave `feature-2` / `b*.md` available for the revision
    // without name collisions in the contributor's clone.
    let original = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                branch: Some("feature-1".into()),
                commits: vec![
                    ("a3.md".into(), "some content\n".into()),
                    ("a4.md".into(), "some content\n".into()),
                ],
                cover_letter: Some((ORIGINAL_TITLE.into(), ORIGINAL_DESCRIPTION.into())),
                ..Default::default()
            },
        )
        .await?;
    let original_cover_letter = original
        .cover_letter_event
        .clone()
        .context("original proposal did not produce a cover-letter event")?;

    // --- revision ----------------------------------------------------------
    //
    // `--in-reply-to <original_cover_letter_id_hex>` marks the new series
    // as a revision of the original. The revision is signed by a fresh
    // contributor — `publish_patch_series` mints one per call. ngit
    // resolves `--in-reply-to` by event id and does not check author
    // identity, so this is fine.
    let revision = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                branch: Some("feature-2".into()),
                commits: vec![
                    ("b3.md".into(), "some content\n".into()),
                    ("b4.md".into(), "some content\n".into()),
                ],
                cover_letter: Some((REVISION_TITLE.into(), REVISION_DESCRIPTION.into())),
                in_reply_to: vec![original_cover_letter.id.to_hex()],
            },
        )
        .await?;

    let revision_cover_letter = revision
        .cover_letter_event
        .clone()
        .context("revision did not produce a cover-letter event")?;
    let first_commit_oid = revision
        .commits
        .first()
        .cloned()
        .context("revision returned no commits")?;
    let revision_patch = find_patch_for_commit(&revision.patch_events, &first_commit_oid)?;

    Ok(Snapshot {
        revision_cover_letter,
        revision_patch,
        original_cover_letter_id: original_cover_letter.id,
    })
}

fn find_patch_for_commit(patches: &[Event], oid: &str) -> Result<Event> {
    patches
        .iter()
        .find(|e| tag_value(e, "commit").as_deref() == Some(oid))
        .cloned()
        .ok_or_else(|| anyhow!("no patch event found with commit tag {oid}"))
}

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
    /// Revision cover letter still carries `["t", "root"]` — a revision
    /// *is* a new proposal root from the threading perspective, even
    /// though it also carries a back-reference to the original via
    /// `revision-root`.
    TTagRoot,
    /// Revision cover letter additionally carries `["t", "revision-root"]`
    /// (or the legacy alias `"root-revision"`). This is the marker
    /// downstream clients use to distinguish revisions from first-time
    /// proposals.
    TTagRevisionRoot,
    /// Revision cover letter carries a 4-slot
    /// `["e", <original_cover_letter_id>, _, "reply"]` tag pointing at
    /// the original proposal's root. This is how a revision references
    /// what it revises.
    ETagReplyToOriginal,
    /// Per-commit patches in the revision thread off the **revision's
    /// own** cover letter via a 4-slot `["e", <cover_letter_id>, _,
    /// "root"]` tag — same shape as the per-commit-patch chaining in a
    /// non-revision cover-lettered series.
    PatchETagRootIsRevisionCoverLetter,
}

#[rstest]
#[case::t_tag_root(Case::TTagRoot)]
#[case::t_tag_revision_root(Case::TTagRevisionRoot)]
#[case::e_tag_reply_to_original(Case::ETagReplyToOriginal)]
#[case::patch_e_tag_root_is_revision_cover_letter(Case::PatchETagRootIsRevisionCoverLetter)]
#[tokio::test]
async fn revision_tags(#[future] snapshot: Snapshot, #[case] case: Case) -> Result<()> {
    let s = snapshot.await;
    match case {
        Case::TTagRoot => {
            assert!(
                tag_values(&s.revision_cover_letter, "t")
                    .iter()
                    .any(|v| v == "root"),
                "revision cover letter should carry `t root`; tags were {:?}",
                s.revision_cover_letter.tags,
            );
        }
        Case::TTagRevisionRoot => {
            // Accept either spelling — `src/lib/git_events.rs` has
            // historically emitted both, and the legacy assertion was
            // also tolerant.
            let t_values = tag_values(&s.revision_cover_letter, "t");
            assert!(
                t_values
                    .iter()
                    .any(|v| v == "revision-root" || v == "root-revision"),
                "revision cover letter should carry `t revision-root` (or the alias \
                 `root-revision`); t tags were {t_values:?}",
            );
        }
        Case::ETagReplyToOriginal => {
            let reply_e = s
                .revision_cover_letter
                .tags
                .iter()
                .find(|t| {
                    let v = t.as_slice();
                    v.first().map(String::as_str) == Some("e")
                        && v.len() == 4
                        && v.get(3).map(String::as_str) == Some("reply")
                })
                .ok_or_else(|| {
                    anyhow!(
                        "revision cover letter missing reply-marker `e` tag (tags: {:?})",
                        s.revision_cover_letter.tags
                    )
                })?;
            assert_eq!(
                reply_e.as_slice().get(1).map(String::as_str),
                Some(s.original_cover_letter_id.to_hex().as_str()),
                "revision cover letter's `e <reply>` should point at the original proposal's root",
            );
        }
        Case::PatchETagRootIsRevisionCoverLetter => {
            let root_e = s
                .revision_patch
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
                        "revision per-commit patch missing root-marker `e` tag (tags: {:?})",
                        s.revision_patch.tags
                    )
                })?;
            assert_eq!(
                root_e.as_slice().get(1).map(String::as_str),
                Some(s.revision_cover_letter.id.to_hex().as_str()),
                "revision per-commit patch's `e <root>` should point at the revision's own \
                 cover letter (not the original proposal's)",
            );
        }
    }
    Ok(())
}
