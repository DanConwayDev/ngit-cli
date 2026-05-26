//! Verifies that `-o title=` / `-o description=` push options override the PR
//! event's `subject` tag and `content` field, and that `\n` (backslash + `n`)
//! in the description value is decoded into actual newline characters before
//! being stored in the event.
//!
//! ## What is being tested
//!
//! `git push -o 'title=bla' -o 'description=bla\n\ntest' origin pr/<branch>`
//! routes through `git-remote-nostr`'s push-option parsing path
//! (`main.rs : ["option", "push-option", …]`).  The raw option string
//! `description=bla\n\ntest` (where `\n` is the literal two-byte sequence
//! backslash + 'n') is passed to `decode_push_option_escapes`, which converts
//! each `\n` sequence into a real U+000A newline character.  The resulting
//! strings are handed to `generate_unsigned_pr_or_update_event` as
//! `title_description_overide`, bypassing the default commit-message
//! extraction path.
//!
//! ## Arrangement
//!
//! 1. Harness: one vanilla relay (`"default"`) + one GRASP server (`"repo"`).
//! 2. Maintainer publishes the repo via [`Harness::publish_repo`].
//! 3. A fresh contributor clones and logs in as a new account.
//! 4. Contributor checks out a `pr/feature` branch and makes two commits with
//!    plain commit messages (not used for the PR title/description here).
//! 5. Contributor runs: `git push -o title=bla -o 'description=bla\n\ntest' -u
//!    origin pr/feature` (via [`Repo::nostr_push`] for timing safety; the `\n`
//!    sequences in the description option string are literal backslash-n
//!    characters that `decode_push_option_escapes` turns into real newlines).
//! 6. [`capture_snapshot`] reads all observable side-effects into a
//!    [`Snapshot`].  The harness then drops.
//! 7. Each `#[rstest]` case asserts on a different slice of the snapshot.
//!
//! ## Coverage (one `#[rstest]` per bullet)
//!
//! 1. Exactly one KIND_PULL_REQUEST event on the GRASP.
//! 2. Zero Kind::GitPatch events.
//! 3. Zero KIND_PULL_REQUEST_UPDATE events.
//! 4. Contributor's `refs/remotes/origin/pr/feature` matches the local tip.
//! 5. Contributor's upstream tracking config is set (`-u` behaviour).
//! 6. GRASP bare repo has `refs/nostr/<pr_event_id>` resolving to the tip.
//! 7. PR event `branch-name` tag equals `"feature"`.
//! 8. PR event `c` tag equals the pushed tip OID.
//! 9. PR event `a` tag encodes the correct 30617 coordinate.
//! 10. A fresh nostr-URL clone lists the branch as `pr/feature(<8-hex>)`.
//! 11. PR event `subject` tag equals `PR_TITLE` (the `-o title=` value), NOT
//!     the first commit's summary — confirming the override path fired.
//! 12. PR event `content` equals `PR_DESCRIPTION` with **real** newlines (i.e.
//!     `bla` + LF + LF + `test`), confirming that `\n` sequences in the `-o
//!     description=` value were decoded by `decode_push_option_escapes`.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for the test repo — distinct from other test repos to avoid
/// cross-test relay pollution on the shared vanilla relay.
const IDENTIFIER: &str = "git-push-pr-custom-subject-desc";

/// Feature branch name; pushed as `pr/feature`.
const BRANCH: &str = "feature";

/// Title passed via `-o title=bla`.  The PR event's `subject` tag must equal
/// this value (case 11), overriding the default commit-message extraction.
const PR_TITLE: &str = "bla";

/// Description passed via `-o description=bla\n\ntest` — but with the `\n`
/// sequences decoded into real U+000A newline characters by
/// `decode_push_option_escapes`.  The PR event's `content` field must equal
/// this value (case 12).
const PR_DESCRIPTION: &str = "bla\n\ntest";

// ---------------------------------------------------------------------------
// Snapshot — captured side-effects of one push with custom title/description
// ---------------------------------------------------------------------------

struct Snapshot {
    pr_event: Event,
    pr_count: usize,
    patch_count: usize,
    pr_update_count: usize,
    contributor_tip_oid: String,
    contributor_remote_tracking_oid: String,
    upstream_merge_cfg: String,
    grasp_pr_ref_oid: String,
    maintainer_pubkey: PublicKey,
    identifier: String,
    pr_event_id_hex: String,
    nostr_clone_ls_refs: BTreeMap<String, String>,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot().await.expect(
                    "git_push_pr::new_pr_custom_subject_desc fixture: capture_snapshot failed",
                ),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- 1. Harness ----------------------------------------------------------
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    // --- 2. Maintainer publishes the repo ------------------------------------
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("git push pr custom desc maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    let maintainer_pubkey = published.maintainer_keys.public_key();

    // --- 3. Clone as a fresh contributor -------------------------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "git push pr custom desc contributor".into(),
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

    // --- 4. Contributor: check out pr/feature and make two commits -----------
    //
    // The commit messages are intentionally plain ("add t1.md", "add t2.md").
    // They are NOT used for the PR title/description because the `-o title=`
    // and `-o description=` push options override that default path.
    contributor
        .git_ok(
            ["checkout", "-b", &format!("pr/{BRANCH}")],
            &format!("git checkout -b pr/{BRANCH}"),
        )
        .await?;

    std::fs::write(contributor.dir().join("t1.md"), "some content\n")
        .context("failed to write t1.md")?;
    contributor
        .git_ok(["add", "t1.md"], "git add t1.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t1.md", "--no-gpg-sign"],
            "git commit t1.md",
        )
        .await?;

    std::fs::write(contributor.dir().join("t2.md"), "some content\n")
        .context("failed to write t2.md")?;
    contributor
        .git_ok(["add", "t2.md"], "git add t2.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t2.md", "--no-gpg-sign"],
            "git commit t2.md",
        )
        .await?;

    let contributor_tip_oid = contributor
        .rev_parse("HEAD")
        .await
        .context("rev-parse HEAD after second commit")?;

    // --- 5. Contributor: push with custom title and description --------------
    //
    // `-o title=bla` sets the PR subject tag to "bla".
    // `-o description=bla\n\ntest` passes the literal two-byte sequences `\n`
    // which `decode_push_option_escapes` (main.rs) converts to real newlines,
    // yielding "bla\n\ntest" (with actual U+000A characters) as the event
    // content.  This is the code path under test.
    contributor
        .nostr_push([
            "-o",
            "title=bla",
            "-o",
            r"description=bla\n\ntest",
            "-u",
            "origin",
            &format!("pr/{BRANCH}"),
        ])
        .await
        .context("nostr_push with custom title/description failed")?;

    // --- 6. Capture contributor local state ----------------------------------
    let contributor_snap = contributor
        .snapshot()
        .context("capturing contributor snapshot after push")?;
    let remote_tracking_ref = format!("refs/remotes/origin/pr/{BRANCH}");
    let contributor_remote_tracking_oid = contributor_snap
        .refs
        .get(&remote_tracking_ref)
        .with_context(|| {
            format!(
                "{remote_tracking_ref} missing from contributor refs after push — \
                 update_remote_refs_pushed (push.rs:165-170) did not run"
            )
        })?
        .clone();

    let upstream_merge_cfg = contributor
        .config(&format!("branch.pr/{BRANCH}.merge"))
        .await?
        .with_context(|| {
            format!(
                "branch.pr/{BRANCH}.merge not set after `git push -u` — \
                 the -u flag did not write upstream tracking config"
            )
        })?;

    // --- 7. Capture events from the GRASP and relay --------------------------
    let pr_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_count = pr_events.len();
    let pr_event = pr_events
        .into_iter()
        .find(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .context(
            "no KIND_PULL_REQUEST with branch-name=\"feature\" authored by contributor \
             found on GRASP after `git push pr/feature`",
        )?;
    let pr_event_id_hex = pr_event.id.to_hex();

    let patch_count = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(Kind::GitPatch),
        )
        .await?
        .len()
        + harness
            .relay("default")
            .events(
                Filter::new()
                    .author(contributor_pubkey)
                    .kind(Kind::GitPatch),
            )
            .await?
            .len();

    let pr_update_count = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?
        .len()
        + harness
            .relay("default")
            .events(
                Filter::new()
                    .author(contributor_pubkey)
                    .kind(KIND_PULL_REQUEST_UPDATE),
            )
            .await?
            .len();

    // --- 8. Read the GRASP bare-repo ref before the harness drops ------------
    let grasp_pr_ref_oid = harness
        .grasp("repo")
        .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &pr_event_id_hex)
        .await?;

    // --- 9. Fresh nostr-URL clone: run git ls-remote -------------------------
    let new_clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    let ls_out = new_clone
        .git(["ls-remote", "origin"])
        .output()
        .await
        .context("failed to spawn git ls-remote origin")?;
    anyhow::ensure!(
        ls_out.status.success(),
        "git ls-remote origin exited {:?}\nstdout: {}\nstderr: {}",
        ls_out.status,
        String::from_utf8_lossy(&ls_out.stdout),
        String::from_utf8_lossy(&ls_out.stderr),
    );
    let ls_stdout =
        String::from_utf8(ls_out.stdout).context("git ls-remote origin stdout is not UTF-8")?;
    let nostr_clone_ls_refs: BTreeMap<String, String> = ls_stdout
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("ref: "))
        .filter_map(|l| l.split_once('\t'))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect();

    Ok(Snapshot {
        pr_event,
        pr_count,
        patch_count,
        pr_update_count,
        contributor_tip_oid,
        contributor_remote_tracking_oid,
        upstream_merge_cfg,
        grasp_pr_ref_oid,
        maintainer_pubkey,
        identifier: IDENTIFIER.to_string(),
        pr_event_id_hex,
        nostr_clone_ls_refs,
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per property
// ---------------------------------------------------------------------------

/// Case 1: Exactly one KIND_PULL_REQUEST event is published on the GRASP.
#[rstest]
#[tokio::test]
async fn pr_event_exactly_one(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 1,
        "expected exactly one KIND_PULL_REQUEST on the GRASP authored by contributor; got {}",
        s.pr_count,
    );
    Ok(())
}

/// Case 2: Zero Kind::GitPatch events on either surface.
#[rstest]
#[tokio::test]
async fn zero_patch_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.patch_count, 0,
        "expected zero Kind::GitPatch events across GRASP and default relay; got {}",
        s.patch_count,
    );
    Ok(())
}

/// Case 3: Zero KIND_PULL_REQUEST_UPDATE events on either surface.
#[rstest]
#[tokio::test]
async fn zero_pr_update_events(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_update_count, 0,
        "expected zero KIND_PULL_REQUEST_UPDATE events; got {}",
        s.pr_update_count,
    );
    Ok(())
}

/// Case 4: Contributor's `refs/remotes/origin/pr/feature` matches the pushed
/// tip OID.
#[rstest]
#[tokio::test]
async fn contributor_pr_remote_tracking_matches_local(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.contributor_remote_tracking_oid, s.contributor_tip_oid,
        "contributor refs/remotes/origin/pr/{BRANCH} ({}) does not match local tip ({})",
        s.contributor_remote_tracking_oid, s.contributor_tip_oid,
    );
    Ok(())
}

/// Case 5: `git push -u` wrote `branch.pr/feature.merge =
/// refs/heads/pr/feature` into the contributor's local config.
#[rstest]
#[tokio::test]
async fn upstream_tracking_config_set(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let expected = format!("refs/heads/pr/{BRANCH}");
    assert_eq!(
        s.upstream_merge_cfg, expected,
        "branch.pr/{BRANCH}.merge = {:?}, expected {:?}",
        s.upstream_merge_cfg, expected,
    );
    Ok(())
}

/// Case 6: The GRASP bare repo has `refs/nostr/<pr_event_id>` resolving to the
/// contributor's tip OID.
#[rstest]
#[tokio::test]
async fn grasp_has_refs_nostr_for_pr(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.grasp_pr_ref_oid,
        s.contributor_tip_oid,
        "GRASP refs/nostr/{} resolves to {} but expected tip {}",
        &s.pr_event_id_hex[..16],
        s.grasp_pr_ref_oid,
        s.contributor_tip_oid,
    );
    Ok(())
}

/// Case 7: PR event `branch-name` tag equals `"feature"`.
#[rstest]
#[tokio::test]
async fn pr_event_branch_name_tag_is_feature(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "branch-name").as_deref(),
        Some(BRANCH),
        "PR event branch-name tag should be {:?}; got {:?}",
        BRANCH,
        tag_value(&s.pr_event, "branch-name"),
    );
    Ok(())
}

/// Case 8: PR event `c` tag equals the contributor's tip OID.
#[rstest]
#[tokio::test]
async fn pr_event_c_tag_is_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "c").as_deref(),
        Some(s.contributor_tip_oid.as_str()),
        "PR event c tag should equal contributor's tip OID; got {:?}, want {:?}",
        tag_value(&s.pr_event, "c"),
        s.contributor_tip_oid,
    );
    Ok(())
}

/// Case 9: PR event has an `a` tag encoding the correct 30617 coordinate.
#[rstest]
#[tokio::test]
async fn pr_event_a_tag_is_repo_coordinate(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let expected = format!("30617:{}:{}", s.maintainer_pubkey, s.identifier);
    let a_tags: Vec<&Tag> = s
        .pr_event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("a"))
        .collect();
    assert!(
        a_tags
            .iter()
            .any(|t| t.as_slice().get(1).map(String::as_str) == Some(expected.as_str())),
        "expected an `a` tag with value {expected:?}; found a tags: {:?}",
        a_tags
            .iter()
            .filter_map(|t| t.as_slice().get(1).cloned())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// Case 10: A fresh nostr-URL clone (not logged in as the contributor) sees
/// the branch listed as `pr/feature(<8-hex-shorthand>)` in
/// `git ls-remote origin` output.
#[rstest]
#[tokio::test]
async fn new_clone_lists_pr_feature_branch(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let shorthand = &s.pr_event_id_hex[..8];
    let expected_ref = format!("refs/heads/pr/{BRANCH}({shorthand})");
    let got_oid = s.nostr_clone_ls_refs.get(&expected_ref).cloned();
    assert_eq!(
        got_oid.as_deref(),
        Some(s.contributor_tip_oid.as_str()),
        "expected fresh clone ls-remote to contain {expected_ref} → {}; \
         got {:?}\nfull ls-remote map: {:#?}",
        s.contributor_tip_oid,
        got_oid,
        s.nostr_clone_ls_refs,
    );
    Ok(())
}

/// Case 11: PR event `subject` tag equals `PR_TITLE` ("bla"), NOT the first
/// commit's message.
///
/// When `-o title=bla` is provided, `generate_unsigned_pr_or_update_event`
/// takes the `title_description_overide` branch (git_events.rs:493-494)
/// instead of calling `get_commit_message_summary`.  This assertion confirms
/// the override path fired correctly.
#[rstest]
#[tokio::test]
async fn pr_event_subject_is_custom_title(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_event, "subject").as_deref(),
        Some(PR_TITLE),
        "PR event subject tag should be the custom title {:?}; got {:?}",
        PR_TITLE,
        tag_value(&s.pr_event, "subject"),
    );
    Ok(())
}

/// Case 12: PR event `content` equals `PR_DESCRIPTION` with real newlines
/// (`"bla\n\ntest"` where `\n` is U+000A).
///
/// The push option is passed as the raw string `description=bla\n\ntest`
/// (literal backslash + 'n' sequences).  `decode_push_option_escapes`
/// (git_remote_nostr/main.rs) converts each `\n` pair into a real newline
/// character.  The resulting string is stored as the PR event content via
/// `generate_unsigned_pr_or_update_event` (git_events.rs:501-502).  An
/// incorrect value would indicate the escape-decoding or override path broke.
#[rstest]
#[tokio::test]
async fn pr_event_content_is_custom_description_with_real_newlines(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_event.content, PR_DESCRIPTION,
        "PR event content should equal custom description {:?} (with real newlines); got {:?}",
        PR_DESCRIPTION, s.pr_event.content,
    );
    Ok(())
}
