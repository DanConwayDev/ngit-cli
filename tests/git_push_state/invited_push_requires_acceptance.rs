//! Regression coverage for explicit co-maintainer acceptance before push.
//!
//! A maintainer can list another pubkey in the repository announcement before
//! that user has published their own kind-30617. Branch and tag pushes are
//! allowed only for confirmed maintainers. A push by an invitee must fail and
//! must not publish an acceptance announcement or repository state; the user
//! accepts deliberately with `ngit repo accept`.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{CloneLogin, Harness, KIND_REPO_STATE, PublishRepoOpts, tag_value};

const BRANCH: &str = "co-maintainer-branch";

#[tokio::test]
async fn invited_co_maintainer_must_accept_before_pushing_state() -> Result<()> {
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
            display_name: Some("invited branch push".into()),
            identifier: Some("invited-branch-push".into()),
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
    let origin_url_before = co_maintainer
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing before rejected push")?;
    let nostr_repo_before = co_maintainer.config("nostr.repo").await?;

    let push = co_maintainer
        .nostr_push_expecting_failure(["-u", "origin", BRANCH])
        .await
        .context("invited co-maintainer push should be rejected")?;
    assert!(!push.status.success());

    let grasp = harness.grasp("repo");
    let grasp_announcements = grasp
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let default_announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    assert!(
        grasp_announcements
            .iter()
            .chain(default_announcements.iter())
            .all(|event| tag_value(event, "d").as_deref() != Some(published.identifier.as_str())),
        "rejected push must not publish an acceptance announcement",
    );

    let origin_url_after = co_maintainer
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing after rejected push")?;
    assert_eq!(
        origin_url_after, origin_url_before,
        "rejected push must leave the origin remote untouched",
    );
    let nostr_repo_after = co_maintainer.config("nostr.repo").await?;
    assert_eq!(
        nostr_repo_after, nostr_repo_before,
        "rejected push must leave nostr.repo untouched",
    );

    let state_events = grasp
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(KIND_REPO_STATE),
        )
        .await?;
    let default_state_events = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(KIND_REPO_STATE),
        )
        .await?;
    assert!(
        state_events
            .iter()
            .chain(default_state_events.iter())
            .all(|event| tag_value(event, "d").as_deref() != Some(published.identifier.as_str())),
        "rejected push must not publish repository state",
    );

    Ok(())
}
