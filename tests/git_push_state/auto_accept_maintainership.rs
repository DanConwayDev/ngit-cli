//! Regression coverage for the `git-remote-nostr push` co-maintainer
//! auto-accept path.
//!
//! A maintainer can list another pubkey in the repository announcement before
//! that user has published their own kind-30617. Branch and tag pushes are
//! allowed only for maintainers, but publishing state under the invited user's
//! pubkey is safe only after they have their own announcement. The push path
//! therefore auto-accepts: it publishes the invited user's announcement with
//! defaults, then continues with the branch push.
//!
//! Auto-accepting must NOT re-root local resolution on the accepter's own
//! coordinate: `nostr.repo` and the origin remote stay exactly as they were,
//! so the repo keeps resolving from the inviter's coordinate and a later
//! removal of the accepter remains observable.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{CloneLogin, Harness, KIND_REPO_STATE, PublishRepoOpts, tag_value, tag_values};

const BRANCH: &str = "co-maintainer-branch";
const BRANCH_REF: &str = "refs/heads/co-maintainer-branch";

#[tokio::test]
async fn invited_co_maintainer_pushes_branch_and_auto_accepts() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("auto-accept branch push".into()),
            identifier: Some("auto-accept-branch-push".into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;

    let co_maintainer_keys = published
        .additional_maintainer_keys
        .first()
        .context("publish_repo did not mint a co-maintainer key")?;
    let co_maintainer_pubkey = co_maintainer_keys.public_key();
    let co_maintainer_nsec = co_maintainer_keys.secret_key().to_bech32()?;

    let co_maintainer = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await
        .context("clone published repo as invited co-maintainer")?;

    co_maintainer
        .git_ok(
            ["config", "--local", "nostr.nsec", &co_maintainer_nsec],
            "git config nostr.nsec (login as invited co-maintainer)",
        )
        .await?;
    co_maintainer
        .git_ok(["checkout", "-b", BRANCH], "git checkout invited branch")
        .await?;

    std::fs::write(
        co_maintainer.dir().join("co-maintainer.txt"),
        "pushed by invited co-maintainer\n",
    )
    .context("write co-maintainer branch file")?;
    co_maintainer
        .git_ok(["add", "co-maintainer.txt"], "git add co-maintainer file")
        .await?;
    co_maintainer
        .git_ok(
            ["commit", "-m", "co-maintainer branch", "--no-gpg-sign"],
            "git commit co-maintainer branch",
        )
        .await?;
    let branch_oid = co_maintainer
        .rev_parse("HEAD")
        .await
        .context("resolve co-maintainer branch tip")?;

    let origin_url_before = co_maintainer
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing before auto-accept push")?;
    let nostr_repo_before = co_maintainer.config("nostr.repo").await?;

    co_maintainer
        .nostr_push(["-u", "origin", BRANCH])
        .await
        .context("invited co-maintainer git push -u origin branch")?;

    let grasp = harness.grasp("repo");
    let announcements = grasp
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let co_maintainer_announcement = announcements
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("co-maintainer kind-30617 was not auto-published on branch push")?;

    let maintainers = tag_values(co_maintainer_announcement, "maintainers");
    assert!(
        maintainers.contains(&co_maintainer_pubkey.to_string()),
        "auto-published announcement should list the co-maintainer; got {maintainers:?}",
    );
    assert!(
        maintainers.contains(&published.maintainer_keys.public_key().to_string()),
        "auto-published announcement should retain the inviting maintainer; got {maintainers:?}",
    );

    let origin_url_after = co_maintainer
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing after auto-accept")?;
    assert_eq!(
        origin_url_after, origin_url_before,
        "auto-accept must leave the origin remote untouched so resolution \
         stays rooted on the inviter's coordinate",
    );
    let nostr_repo_after = co_maintainer.config("nostr.repo").await?;
    assert_eq!(
        nostr_repo_after, nostr_repo_before,
        "auto-accept must not write nostr.repo; re-rooting resolution on the \
         accepter's own coordinate would hide a later removal",
    );

    let state_events = grasp
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(KIND_REPO_STATE),
        )
        .await?;
    let state_event = state_events
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("branch push did not publish a co-maintainer-authored state event")?;
    assert_eq!(
        tag_value(state_event, BRANCH_REF).as_deref(),
        Some(branch_oid.as_str()),
        "state event should record the pushed co-maintainer branch",
    );

    Ok(())
}
