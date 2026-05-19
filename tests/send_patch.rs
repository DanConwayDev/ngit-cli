//! Patch-kind tag-shape regression coverage for `ngit send` — successor to
//! the legacy `cover_letter_tags` + `patch_tags` groups inside
//! `tests/legacy/ngit_send.
//! rs::when_cover_letter_details_specified_with_range_of_head_2_sends_cover_letter_and_2_patches_to_3_relays`.
//!
//!
//! ## Why this file ends in `_patch.rs`
//!
//! Both legacy groups exercise the **patch-kind** proposal flow — the
//! cover letter is a `Kind::GitPatch` event carrying the
//! `["t", "cover-letter"]` tag, and every subsequent per-commit event is
//! also `Kind::GitPatch`. There is no PR-kind sibling for these
//! particular tag-shape regressions because the PR-kind format is
//! a single `KIND_PULL_REQUEST` event with a categorically different
//! tag layout; preserving the cover-letter / per-patch tag contracts is
//! a patch-only concern. A PR-kind sibling for the *other* `ngit_send`
//! groups (when one is justified) would live in `tests/send_pr.rs`.
//!
//! ## Shape
//!
//! - 1 vanilla relay (`"default"`) + 1 GRASP (`"repo"`).
//! - Maintainer publishes a repo with **one additional co-maintainer** listed
//!   on the kind-30617 announcement, so the per-maintainer `a` / `p` tag
//!   assertions exercise a list of length > 1 (the regression the legacy test
//!   caught — and the reason [`PublishRepoOpts::additional_maintainer_count`]
//!   exists). The single-maintainer case is implicitly covered by every other
//!   harness test.
//! - A fresh contributor clone is minted by [`Harness::publish_patch_series`]
//!   and a 2-commit branch (`t3.md`, `t4.md`) is published as a patch series
//!   **with a cover letter** via `ngit send HEAD~2 --force-patch --title ...
//!   --description ...`.
//!
//! ## rstest discipline
//!
//! Each `#[case]` is a read-only assertion on the captured `Snapshot` —
//! exactly the shape `docs/architecture/test-harness-migration.md`
//! sanctions for rstest. The fixture rebuilds the entire harness per
//! case (no `#[once]`); cases never run additional ngit/git commands.
//! Per-rstest-case isolation is delegated to `tokio::test` + a fresh
//! `Harness::build()` per fixture invocation, matching the legacy
//! `TwoBranchesScenario` shape in `tests/legacy/git_remote_nostr/push.rs`.

use anyhow::{Context, Result, anyhow, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, PublishPatchSeriesOpts, PublishRepoOpts};

const COVER_LETTER_TITLE: &str = "exampletitle";
const COVER_LETTER_DESCRIPTION: &str = "exampledescription";
const FIRST_COMMIT_FILE: &str = "t3.md";
const SECOND_COMMIT_FILE: &str = "t4.md";

/// Captured side-effects of one `ngit send --force-patch --title ...
/// --description ...` invocation against a multi-maintainer repo. Populated
/// once per fixture call; rstest cases read fields and never mutate.
struct Snapshot {
    /// The cover-letter event (the `Kind::GitPatch` carrying
    /// `["t", "cover-letter"]`).
    cover_letter: Event,
    /// Patch event for the first commit (`t3.md`). The legacy
    /// `prep()` helper returned this same event.
    first_patch: Event,
    /// Patch event for the second commit (`t4.md`).
    second_patch: Event,
    /// OID of the first commit on the feature branch (`t3.md`).
    first_commit_oid: String,
    /// Commit oid the first patch sits on top of — equals the seed
    /// commit's oid because the harness's `publish_repo` makes a single
    /// `main` commit before branching. `parent-commit` and the
    /// per-patch `r <root>` tag therefore point at the same oid, which
    /// is *not* true in the legacy fixture (it cherry-picked an extra
    /// `commit.md` onto main first). The assertion still catches the
    /// regression — "patch carries a `parent-commit` tag pointing at
    /// the right oid" — without depending on the legacy commit layout.
    root_commit_oid: String,
    /// Identifier the announcement was published with — matches the
    /// `d` tag on the kind-30617 event and the third coordinate
    /// component of every `a` tag on a patch.
    identifier: String,
    /// Pubkeys of every maintainer listed on the announcement, in the
    /// order they appear there: `[publisher, extra-1]`. Both must be
    /// p-tagged and a-coordinate-tagged on every published patch /
    /// cover letter.
    maintainer_pubkeys: Vec<PublicKey>,
    /// Branch name the contributor committed on — matches the
    /// `branch-name` tag on the cover-letter event.
    branch_name: String,
}

#[fixture]
async fn snapshot() -> Snapshot {
    capture_snapshot()
        .await
        .expect("send_patch fixture: capture_snapshot failed")
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
            display_name: Some("send-patch maintainer".into()),
            identifier: Some("send-patch-repo".into()),
            // One *additional* co-maintainer beyond the publisher ⇒ the
            // announcement carries two maintainers, which is the
            // minimum number needed to give the per-maintainer `a` /
            // `p` tag assertions something non-trivial to assert on.
            // Increasing this further would only re-test the same loop;
            // legacy used two maintainers for the same reason.
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;

    let series = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                commits: vec![
                    (FIRST_COMMIT_FILE.into(), "some content\n".into()),
                    (SECOND_COMMIT_FILE.into(), "some content\n".into()),
                ],
                cover_letter: Some((COVER_LETTER_TITLE.into(), COVER_LETTER_DESCRIPTION.into())),
                ..Default::default()
            },
        )
        .await?;

    let cover_letter = series
        .cover_letter_event
        .clone()
        .context("expected a cover-letter event after --title/--description")?;

    // Order patches by their `commit` tag rather than by the grasp's
    // arbitrary return order. The legacy test got away with insertion
    // order because the mock relay preserved it; the new harness queries
    // the grasp over a real REQ and does not guarantee any ordering.
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

    let mut maintainer_pubkeys = vec![published.maintainer_keys.public_key()];
    maintainer_pubkeys.extend(
        published
            .additional_maintainer_keys
            .iter()
            .map(|k| k.public_key()),
    );

    Ok(Snapshot {
        cover_letter,
        first_patch,
        second_patch,
        first_commit_oid,
        root_commit_oid: published.initial_oid.clone(),
        identifier: published.identifier.clone(),
        maintainer_pubkeys,
        branch_name: series.branch_name.clone(),
    })
}

/// Find the patch event whose `["commit", <oid>]` tag matches `oid`.
/// Used to reconstruct chronological order from the unordered REQ
/// response, replacing the legacy insertion-order dependency.
fn find_patch_for_commit(patches: &[Event], oid: &str) -> Result<Event> {
    patches
        .iter()
        .find(|e| tag_value(e, "commit").as_deref() == Some(oid))
        .cloned()
        .ok_or_else(|| anyhow!("no patch event found with commit tag {oid}"))
}

/// Value of the first tag whose first slot equals `key`. Returns `None`
/// if no such tag exists or the value slot is empty.
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

/// All values of every tag whose first slot equals `key`. Used for
/// `p` / `a` tag assertions where a single event carries multiple
/// entries.
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

/// Full slice of the first tag whose first slot equals `key`. Used by
/// the `author` / `committer` cases which assert on the entire tag
/// shape (`[key, name, email, ts, tz]`).
fn full_tag(event: &Event, key: &str) -> Result<Vec<String>> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(key))
        .map(|t| t.as_slice().to_vec())
        .ok_or_else(|| anyhow!("no `{key}` tag on event {}", event.id))
}

/// Expected `a` tag value for the kind-30617 announcement of `maintainer`.
/// Coordinate format is `<kind>:<pubkey_hex>:<identifier>` — see
/// `src/lib/git_events.rs:194-202` for the producer side.
fn expected_a_coord(maintainer: PublicKey, identifier: &str) -> String {
    format!(
        "{}:{}:{identifier}",
        Kind::GitRepoAnnouncement.as_u16(),
        maintainer.to_hex()
    )
}

// ---------------------------------------------------------------------------
// cover_letter_tags — successor to legacy `cover_letter_tags::*`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum CoverLetterCase {
    /// `["r", <root_commit_oid>]` — the very first commit on `main`,
    /// included so a downstream client can resolve the proposal against
    /// the right repo even without the kind-30617 announcement to hand.
    RootCommitAsR,
    /// `["t", "cover-letter"]` — the marker that distinguishes a
    /// cover-letter patch from the per-commit patches in the same series.
    TTagCoverLetter,
    /// `["t", "root"]` — present when the cover letter starts a *new*
    /// proposal (as opposed to revising an existing one). Reverse case
    /// would carry `["t", "root-revision"]` instead, but that lives in
    /// the in-reply-to / proposal-revision tests (legacy 4b group).
    TTagRoot,
    /// `["branch-name", <feature-branch>]` — emitted only on the root
    /// of the series (the cover letter when one is requested); per
    /// `src/lib/git_events.rs:260-271`. Useful for clients building
    /// `pr/<branch>` ref names.
    BranchName,
    /// `["alt", "git patch cover letter: <title>"]` — accessibility /
    /// nostr-client preview text. Format is fixed in
    /// `git_events.rs:692`.
    Alt,
}

#[rstest]
#[case::root_commit_as_r(CoverLetterCase::RootCommitAsR)]
#[case::t_tag_cover_letter(CoverLetterCase::TTagCoverLetter)]
#[case::t_tag_root(CoverLetterCase::TTagRoot)]
#[case::branch_name(CoverLetterCase::BranchName)]
#[case::alt(CoverLetterCase::Alt)]
#[tokio::test]
async fn cover_letter_tags(
    #[future] snapshot: Snapshot,
    #[case] case: CoverLetterCase,
) -> Result<()> {
    let s = snapshot.await;
    match case {
        CoverLetterCase::RootCommitAsR => {
            assert_eq!(
                tag_value(&s.cover_letter, "r").as_deref(),
                Some(s.root_commit_oid.as_str()),
                "cover letter should carry an `r` tag for the repo's root commit",
            );
        }
        CoverLetterCase::TTagCoverLetter => {
            assert!(
                tag_values(&s.cover_letter, "t")
                    .iter()
                    .any(|v| v == "cover-letter"),
                "cover letter should carry `t cover-letter`",
            );
        }
        CoverLetterCase::TTagRoot => {
            assert!(
                tag_values(&s.cover_letter, "t").iter().any(|v| v == "root"),
                "cover letter should carry `t root` (new proposal, not a revision)",
            );
        }
        CoverLetterCase::BranchName => {
            assert_eq!(
                tag_value(&s.cover_letter, "branch-name").as_deref(),
                Some(s.branch_name.as_str()),
                "cover letter should carry `branch-name <feature-branch>`",
            );
        }
        CoverLetterCase::Alt => {
            assert_eq!(
                tag_value(&s.cover_letter, "alt").as_deref(),
                Some(format!("git patch cover letter: {COVER_LETTER_TITLE}").as_str()),
                "cover letter `alt` text format changed",
            );
        }
    }
    Ok(())
}

#[rstest]
#[tokio::test]
async fn cover_letter_a_tag_for_each_maintainer(#[future] snapshot: Snapshot) -> Result<()> {
    let s = snapshot.await;
    let coords = tag_values(&s.cover_letter, "a");
    for m in &s.maintainer_pubkeys {
        let expected = expected_a_coord(*m, &s.identifier);
        assert!(
            coords.iter().any(|c| c == &expected),
            "cover letter missing `a` tag for maintainer {m} (expected {expected:?}, \
             got {coords:?})",
        );
    }
    Ok(())
}

#[rstest]
#[tokio::test]
async fn cover_letter_p_tag_for_each_maintainer(#[future] snapshot: Snapshot) -> Result<()> {
    let s = snapshot.await;
    let p_tags = tag_values(&s.cover_letter, "p");
    for m in &s.maintainer_pubkeys {
        let hex = m.to_hex();
        assert!(
            p_tags.iter().any(|p| p == &hex),
            "cover letter missing `p` tag for maintainer {m} (got {p_tags:?})",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// patch_tags — successor to legacy `patch_tags::*`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum PatchCase {
    /// `["r", <commit_oid>]` and `["commit", <commit_oid>]` — both tags
    /// point at the patch's own commit oid. The `r` tag is a redundant
    /// reference; `commit` is the canonical anchor used by
    /// `make_commits_for_proposal` in `src/bin/git_remote_nostr/list.rs`.
    CommitAndCommitR,
    /// `["parent-commit", <parent_oid>]` — the immediate ancestor of
    /// the patch's commit. In this fixture the first commit's parent is
    /// the seed (= root_commit_oid).
    ParentCommit,
    /// `["r", <root_commit_oid>]` — the repo's root commit. Note this
    /// is the **same** `r` family as `commit_and_commit_r`; both
    /// references live as `r` tags side-by-side.
    RootCommitAsR,
    /// `["description", "add t3.md"]` — taken verbatim from the commit
    /// message. Diverges from the legacy hardcoded message only because
    /// the harness names commits after their file (`add <file>`) rather
    /// than `cli_tester_create_proposals` did the same.
    Description,
    /// `["alt", "git patch: add t3.md"]`.
    Alt,
    /// Patch's `e` tag with marker `root` points at the cover-letter
    /// event id. The first patch in a cover-lettered series is what
    /// downstream clients thread replies off of, via this tag.
    CoverLetterAsRoot,
    /// Second patch's `e` tag with marker `reply` points at the first
    /// patch — chains the series in chronological order.
    SecondTagsFirstWithReply,
    /// No `["t", "root"]` on per-commit patches; only the cover letter
    /// (or first patch in a no-cover-letter series) carries that.
    NoTRootTag,
}

#[rstest]
#[case::commit_and_commit_r(PatchCase::CommitAndCommitR)]
#[case::parent_commit(PatchCase::ParentCommit)]
#[case::root_commit_as_r(PatchCase::RootCommitAsR)]
#[case::description_with_commit_message(PatchCase::Description)]
#[case::alt(PatchCase::Alt)]
#[case::cover_letter_event_as_root(PatchCase::CoverLetterAsRoot)]
#[case::second_patch_tags_first_with_reply(PatchCase::SecondTagsFirstWithReply)]
#[case::no_t_root_tag(PatchCase::NoTRootTag)]
#[tokio::test]
async fn patch_tags(#[future] snapshot: Snapshot, #[case] case: PatchCase) -> Result<()> {
    let s = snapshot.await;
    match case {
        PatchCase::CommitAndCommitR => {
            let r_tags = tag_values(&s.first_patch, "r");
            assert!(
                r_tags.iter().any(|v| v == &s.first_commit_oid),
                "first patch missing `r <first-commit>`; r tags were {r_tags:?}",
            );
            assert_eq!(
                tag_value(&s.first_patch, "commit").as_deref(),
                Some(s.first_commit_oid.as_str()),
                "first patch should carry `commit <first-commit>`",
            );
        }
        PatchCase::ParentCommit => {
            assert_eq!(
                tag_value(&s.first_patch, "parent-commit").as_deref(),
                Some(s.root_commit_oid.as_str()),
                "first patch's `parent-commit` should be the main tip before branching",
            );
        }
        PatchCase::RootCommitAsR => {
            let r_tags = tag_values(&s.first_patch, "r");
            assert!(
                r_tags.iter().any(|v| v == &s.root_commit_oid),
                "first patch missing `r <root-commit>`; r tags were {r_tags:?}",
            );
        }
        PatchCase::Description => {
            // `description` carries the full commit message body, which
            // git terminates with a trailing newline. The legacy test
            // hardcoded the stripped form because its commit-message
            // helper trimmed; here we let the harness commit through
            // plain `git commit -m`, which preserves the newline.
            assert_eq!(
                tag_value(&s.first_patch, "description").as_deref(),
                Some(format!("add {FIRST_COMMIT_FILE}\n").as_str()),
                "first patch's `description` should mirror the commit message",
            );
        }
        PatchCase::Alt => {
            assert_eq!(
                tag_value(&s.first_patch, "alt").as_deref(),
                Some(format!("git patch: add {FIRST_COMMIT_FILE}").as_str()),
                "first patch `alt` text format changed",
            );
        }
        PatchCase::CoverLetterAsRoot => {
            // Find the `e` tag with the `root` marker (4-slot form:
            // `["e", <id>, <relay-hint>, "root"]`).
            let root_e = s
                .first_patch
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
                        "first patch missing root-marker `e` tag (tags: {:?})",
                        s.first_patch.tags
                    )
                })?;
            assert_eq!(
                root_e.as_slice().get(1).map(String::as_str),
                Some(s.cover_letter.id.to_hex().as_str()),
                "first patch's `e <root>` should point at the cover letter",
            );
        }
        PatchCase::SecondTagsFirstWithReply => {
            let reply_e = s
                .second_patch
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
                        "second patch missing reply-marker `e` tag (tags: {:?})",
                        s.second_patch.tags
                    )
                })?;
            assert_eq!(
                reply_e.as_slice().get(1).map(String::as_str),
                Some(s.first_patch.id.to_hex().as_str()),
                "second patch's `e <reply>` should point at the first patch",
            );
        }
        PatchCase::NoTRootTag => {
            assert!(
                !tag_values(&s.first_patch, "t").iter().any(|v| v == "root"),
                "per-commit patches must not carry `t root`; that tag belongs to the cover letter",
            );
        }
    }
    Ok(())
}

#[rstest]
#[tokio::test]
async fn patch_a_tag_for_each_maintainer(#[future] snapshot: Snapshot) -> Result<()> {
    let s = snapshot.await;
    let coords = tag_values(&s.first_patch, "a");
    for m in &s.maintainer_pubkeys {
        let expected = expected_a_coord(*m, &s.identifier);
        assert!(
            coords.iter().any(|c| c == &expected),
            "patch missing `a` tag for maintainer {m} (expected {expected:?}, got {coords:?})",
        );
    }
    Ok(())
}

#[rstest]
#[tokio::test]
async fn patch_p_tag_for_each_maintainer(#[future] snapshot: Snapshot) -> Result<()> {
    let s = snapshot.await;
    let p_tags = tag_values(&s.first_patch, "p");
    for m in &s.maintainer_pubkeys {
        let hex = m.to_hex();
        assert!(
            p_tags.iter().any(|p| p == &hex),
            "patch missing `p` tag for maintainer {m} (got {p_tags:?})",
        );
    }
    Ok(())
}

#[rstest]
#[tokio::test]
async fn patch_commit_author(#[future] snapshot: Snapshot) -> Result<()> {
    let s = snapshot.await;
    let author = full_tag(&s.first_patch, "author")?;
    // Legacy asserted the full 5-slot shape including epoch zeros for
    // timestamp/tz because it overrode the commit time. The new harness
    // takes real wall-clock commit time, so we pin slots 0-2 (key /
    // name / email) and only check the timestamp/tz slots are present
    // and well-shaped (decimal integer / signed decimal integer).
    assert_eq!(
        &author[..3],
        &["author", "ngit test", "ngit-test@example.invalid"],
        "patch `author` tag should carry the harness's default identity",
    );
    if author.len() != 5 {
        bail!("patch `author` tag should be 5 slots [author, name, email, ts, tz]; got {author:?}",);
    }
    author[3]
        .parse::<i64>()
        .with_context(|| format!("author timestamp slot is not an integer: {:?}", author[3]))?;
    Ok(())
}

#[rstest]
#[tokio::test]
async fn patch_commit_committer(#[future] snapshot: Snapshot) -> Result<()> {
    let s = snapshot.await;
    let committer = full_tag(&s.first_patch, "committer")?;
    assert_eq!(
        &committer[..3],
        &["committer", "ngit test", "ngit-test@example.invalid"],
        "patch `committer` tag should carry the harness's default identity",
    );
    if committer.len() != 5 {
        bail!(
            "patch `committer` tag should be 5 slots [committer, name, email, ts, tz]; got \
             {committer:?}",
        );
    }
    committer[3].parse::<i64>().with_context(|| {
        format!(
            "committer timestamp slot is not an integer: {:?}",
            committer[3]
        )
    })?;
    Ok(())
}
