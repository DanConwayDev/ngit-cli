//! Regression coverage for the state-push success condition when only the
//! initial GRASP relay publish succeeds.
//!
//! A normal state push publishes the new kind-30618 to GRASP relays before
//! pushing git objects, then fans out to any remaining repo relays. If the git
//! push succeeds and that initial GRASP relay publish accepted the state, the
//! remote helper must report success even when a later vanilla relay fanout
//! fails.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, KIND_REPO_STATE, PublishRepoOpts, tag_value};

const IDENTIFIER: &str = "state-push-grasp-ok-remaining-relay-down";

#[tokio::test]
async fn grasp_acceptance_keeps_state_push_success_when_remaining_relay_is_down() -> Result<()> {
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let default_relay_url = harness.relay("default").url().to_string();
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("state push partial relay success".into()),
            identifier: Some(IDENTIFIER.into()),
            extra_repo_relays: vec![default_relay_url],
            ..Default::default()
        })
        .await?;

    let stopped_relay = harness
        .take_relay("default")
        .context("default relay should be registered")?;
    drop(stopped_relay);

    std::fs::write(
        publisher.dir().join("partial-relay.md"),
        "grasp accepts before remaining relay goes away\n",
    )
    .context("failed to write test file")?;
    publisher
        .git_ok(["add", "partial-relay.md"], "git add partial-relay.md")
        .await?;
    publisher
        .git_ok(
            [
                "commit",
                "-m",
                "commit while remaining relay is down",
                "--no-gpg-sign",
            ],
            "git commit partial relay change",
        )
        .await?;
    let new_head = publisher.rev_parse("HEAD").await?;

    publisher.nostr_push(["origin", "main"]).await.context(
        "git push should succeed when GRASP accepted state before vanilla fanout failed",
    )?;

    let origin_main_after = publisher
        .rev_parse("refs/remotes/origin/main")
        .await
        .context("origin/main should advance after successful push")?;
    assert_eq!(
        origin_main_after, new_head,
        "successful push should advance local remote-tracking state",
    );

    let state_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(KIND_REPO_STATE),
        )
        .await?;
    let grasp_has_new_state = state_events.iter().any(|event| {
        tag_value(event, "d").as_deref() == Some(IDENTIFIER)
            && tag_value(event, "refs/heads/main").as_deref() == Some(new_head.as_str())
    });
    assert!(
        grasp_has_new_state,
        "GRASP relay should retain the accepted state event for the pushed branch",
    );

    Ok(())
}
