//! Patch-kind regression coverage for `ngit send --in-reply-to` where the
//! reference is **not** an existing proposal root — i.e. it's an issue
//! event (treated as a mention) or an npub/nprofile (treated as a pubkey
//! mention). Successor to:
//!
//! - `tests/legacy/ngit_send.rs::in_reply_to_mentions_issue::*` (2 tests)
//! - `tests/legacy/ngit_send.
//!   rs::in_reply_to_mentions_npub_and_nprofile_which_get_mentioned_in_proposal_root::*
//!   ` (1 test)
//!
//! ## Why these aren't revisions
//!
//! `src/bin/ngit/sub_commands/send.
//! rs::get_root_proposal_and_mentions_from_in_reply_to` treats the first
//! `--in-reply-to` value as a *potential* proposal root. It's only adopted as a
//! revision target if the event resolves locally **and** is a patch-set root or
//! `KIND_PULL_REQUEST`. Issue events (`Kind::GitIssue`) and pubkey references
//! fail that check, so the parser falls through to the mention path and emits
//! the reference as a tag on the proposal root (cover letter, when one is
//! requested):
//!
//! - Event id → `["q", <id>]` (NIP-21 quote)
//! - npub / nprofile → `["p", <pubkey_hex>]`
//!
//! ## Shape
//!
//! Two distinct scenarios in the same file because both target the
//! `--in-reply-to <mention>` cover-letter contract; splitting would
//! duplicate the surrounding `Harness::builder() + publish_repo`
//! choreography for no signal gain. Each scenario builds a fresh harness
//! per case (no `#[once]`).
//!
//! - **Issue scenario** (2 rstest cases on a shared snapshot): the maintainer
//!   publishes an issue via `ngit issue create`, then a contributor publishes a
//!   cover-lettered patch series with `--in-reply-to <issue_id_hex>`. Cases
//!   assert on cover-letter tags.
//! - **Pubkey-mention scenario** (1 standalone test): the contributor publishes
//!   a cover-lettered patch series with `--in-reply-to <npub> <nprofile>` (two
//!   references at once). The test asserts on cover-letter `p` tags.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, PublishPatchSeriesOpts, PublishRepoOpts, Repo};

const COVER_LETTER_TITLE: &str = "exampletitle";
const COVER_LETTER_DESCRIPTION: &str = "exampledescription";

// ---------------------------------------------------------------------------
// Issue-mention scenario
// ---------------------------------------------------------------------------

/// Captured outputs of one `ngit send --in-reply-to <issue_event_id>` run.
struct IssueSnapshot {
    /// Cover letter of the patch series — the event under test.
    cover_letter: Event,
    /// Id of the kind-1621 issue published by the maintainer before the
    /// patch series; the value the cover letter's `q` tag should
    /// reference.
    issue_event_id: EventId,
}

#[fixture]
async fn issue_snapshot() -> IssueSnapshot {
    capture_issue_snapshot()
        .await
        .expect("send_patch_in_reply_to::issue_snapshot: capture failed")
}

async fn capture_issue_snapshot() -> Result<IssueSnapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    // --- maintainer publishes an issue ------------------------------------
    //
    // `ngit issue create` is the natural way to land a real kind-1621
    // event on the grasp; using the CLI here (rather than fabricating
    // an event through the nostr-sdk Client) keeps the scenario aligned
    // with what an actual user would do, and exercises the issue
    // command's own publish path as a side-effect.
    let issue_event_id = ngit_issue_create(&publisher, "test issue", "for in-reply-to").await?;

    // --- contributor publishes a patch series referencing the issue ------
    //
    // The `--in-reply-to` value is the issue id in raw **hex** form. Both
    // hex and bech32 (`note1...`) routes through
    // `git_events.rs::event_tag_from_nip19_or_hex` now honour
    // `EventRefType::Quote` and emit the expected `["q", <id>]` tag on
    // the cover letter — see the unit tests
    // `event_tag_from_nip19_or_hex::quote_ref_type_emits_q_tag_for_*`
    // for the corresponding lower-level coverage. (Historically the
    // hex branch unconditionally produced `["e", <id>]` regardless of
    // ref_type; this scenario doubles as the integration-level
    // regression guard for that fix.)
    let issue_id_hex = issue_event_id.to_hex();
    let series = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                commits: vec![
                    ("t3.md".into(), "some content\n".into()),
                    ("t4.md".into(), "some content\n".into()),
                ],
                cover_letter: Some((COVER_LETTER_TITLE.into(), COVER_LETTER_DESCRIPTION.into())),
                in_reply_to: vec![issue_id_hex],
                ..Default::default()
            },
        )
        .await?;

    let cover_letter = series
        .cover_letter_event
        .clone()
        .context("contributor patch series did not produce a cover letter")?;

    Ok(IssueSnapshot {
        cover_letter,
        issue_event_id,
    })
}

#[derive(Debug, Clone, Copy)]
enum IssueCase {
    /// Cover letter carries `["q", <issue_event_id>]`. This is the NIP-21
    /// quote tag emitted by `git_events.rs::event_tag_from_nip19_or_hex`
    /// for an event-id reference parsed under `EventRefType::Quote`.
    QTagsIssue,
    /// Cover letter does **not** carry `["t", "revision-root"]` (nor the
    /// legacy alias `"root-revision"`). A revision marker would imply
    /// this proposal revises another proposal, which it doesn't — the
    /// in-reply-to target is an issue, not a proposal root.
    NotTaggedAsRevision,
}

#[rstest]
#[case::q_tags_issue(IssueCase::QTagsIssue)]
#[case::not_tagged_as_revision(IssueCase::NotTaggedAsRevision)]
#[tokio::test]
async fn in_reply_to_issue(
    #[future] issue_snapshot: IssueSnapshot,
    #[case] case: IssueCase,
) -> Result<()> {
    let s = issue_snapshot.await;
    match case {
        IssueCase::QTagsIssue => {
            let q_tags = tag_values(&s.cover_letter, "q");
            let want = s.issue_event_id.to_hex();
            assert!(
                q_tags.iter().any(|v| v == &want),
                "cover letter should carry `q <issue_id>`; q tags were {q_tags:?}, \
                 expected to contain {want}",
            );
        }
        IssueCase::NotTaggedAsRevision => {
            let t_values = tag_values(&s.cover_letter, "t");
            assert!(
                !t_values
                    .iter()
                    .any(|v| v == "revision-root" || v == "root-revision"),
                "cover letter must not carry `t revision-root` when --in-reply-to is an issue \
                 rather than a proposal root; t tags were {t_values:?}",
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// npub / nprofile mention scenario
// ---------------------------------------------------------------------------

#[tokio::test]
async fn in_reply_to_npub_and_nprofile_appear_as_p_tags() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    // Mint two fresh identities to mention. The legacy test used
    // hardcoded npub / nprofile strings; we generate fresh ones so the
    // scenario doesn't depend on out-of-band key material.
    let npub_target = Keys::generate();
    let nprofile_target = Keys::generate();

    let npub_string = npub_target
        .public_key()
        .to_bech32()
        .context("encoding npub")?;
    // `nprofile1...` wraps a pubkey plus optional relay hints. ngit only
    // reads the embedded pubkey when emitting a `p` tag (see
    // `git_events.rs:409-412`), so the relay-hint contents don't affect
    // the assertion — we pass a single placeholder relay url for
    // realism. Any well-formed wss:// url works.
    let nprofile_string = Nip19Profile {
        public_key: nprofile_target.public_key(),
        relays: vec![RelayUrl::parse("wss://example.invalid")?],
    }
    .to_bech32()
    .context("encoding nprofile")?;

    let series = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                commits: vec![
                    ("t3.md".into(), "some content\n".into()),
                    ("t4.md".into(), "some content\n".into()),
                ],
                cover_letter: Some((COVER_LETTER_TITLE.into(), COVER_LETTER_DESCRIPTION.into())),
                in_reply_to: vec![npub_string.clone(), nprofile_string.clone()],
                ..Default::default()
            },
        )
        .await?;

    let cover_letter = series
        .cover_letter_event
        .clone()
        .context("contributor patch series did not produce a cover letter")?;

    let p_tags = tag_values(&cover_letter, "p");
    let want_npub_hex = npub_target.public_key().to_hex();
    let want_nprofile_hex = nprofile_target.public_key().to_hex();

    assert!(
        p_tags.iter().any(|v| v == &want_npub_hex),
        "cover letter should carry `p <pubkey>` for the npub mention; p tags were {p_tags:?}, \
         expected to contain {want_npub_hex}",
    );
    assert!(
        p_tags.iter().any(|v| v == &want_nprofile_hex),
        "cover letter should carry `p <pubkey>` for the nprofile mention; p tags were {p_tags:?}, \
         expected to contain {want_nprofile_hex}",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Run `ngit issue create --subject <title> --body <body>` from a
/// logged-in working tree and parse the resulting event id out of
/// `issue created: <hex>` on stdout. Matches the print format in
/// `src/bin/ngit/sub_commands/issue_create.rs:120` — if that line ever
/// reflows, the parser breaks loudly here rather than in a downstream
/// assertion.
async fn ngit_issue_create(repo: &Repo, title: &str, body: &str) -> Result<EventId> {
    let out = repo
        .ngit(["issue", "create", "--subject", title, "--body", body])
        .output()
        .await
        .context("failed to spawn `ngit issue create`")?;
    if !out.status.success() {
        bail!(
            "`ngit issue create` exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    parse_issue_event_id(&stdout).with_context(|| {
        format!(
            "no `issue created:` line in `ngit issue create` stdout — has the print format \
             changed?\nfull stdout was:\n{stdout}"
        )
    })
}

/// Pull the hex event id from the `issue created: <hex>` line in `ngit
/// issue create` stdout. Case-insensitive prefix match per the harness
/// rules — the only exact-stdout reads we tolerate are sentinel parses
/// like this one (see `AGENTS.md` § "Test harness boundary").
fn parse_issue_event_id(stdout: &str) -> Option<EventId> {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("issue created:") {
            let rest = line[idx + "issue created:".len()..].trim();
            if let Ok(id) = EventId::from_hex(rest) {
                return Some(id);
            }
        }
    }
    None
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
