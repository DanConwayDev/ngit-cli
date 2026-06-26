//! Regression coverage for state pushes when no git server accepts objects.
//!
//! A branch/tag push to a `nostr://` remote must not report remote-helper `ok`
//! just because the state event was signed or cached locally. If no git server
//! accepts the pushed objects, `git push` must fail and the new state must not
//! be fanned out to remaining relays.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, KIND_REPO_STATE, PublishRepoOpts, tag_value, tick_to_next_second};

const IDENTIFIER: &str = "state-push-all-servers-down";

#[tokio::test]
async fn failed_git_server_push_does_not_report_success_or_fan_out_state() -> Result<()> {
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
            display_name: Some("state push all servers down".into()),
            identifier: Some(IDENTIFIER.into()),
            extra_repo_relays: vec![default_relay_url],
            ..Default::default()
        })
        .await?;

    let old_origin_main = publisher
        .rev_parse("refs/remotes/origin/main")
        .await
        .context("origin/main should exist after initial publish")?;

    std::fs::write(publisher.dir().join("offline.md"), "server is offline\n")
        .context("failed to write test file")?;
    publisher
        .git_ok(["add", "offline.md"], "git add offline.md")
        .await?;
    publisher
        .git_ok(
            [
                "commit",
                "-m",
                "commit while server offline",
                "--no-gpg-sign",
            ],
            "git commit offline change",
        )
        .await?;
    let new_head = publisher.rev_parse("HEAD").await?;

    let stopped_grasp = harness
        .take_grasp("repo")
        .context("repo grasp should be registered")?;
    drop(stopped_grasp);

    tick_to_next_second().await;
    let push_out = publisher
        .git(["push", "origin", "main"])
        .output()
        .await
        .context("failed to spawn git push origin main")?;

    assert!(
        !push_out.status.success(),
        "git push unexpectedly succeeded when every git server was down\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&push_out.stdout),
        String::from_utf8_lossy(&push_out.stderr),
    );

    let origin_main_after = publisher
        .rev_parse("refs/remotes/origin/main")
        .await
        .context("origin/main should remain readable after failed push")?;
    assert_eq!(
        origin_main_after, old_origin_main,
        "failed push should not advance local remote-tracking state",
    );

    let state_events = harness
        .relay("default")
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(KIND_REPO_STATE),
        )
        .await?;
    let fanned_out_new_state = state_events.iter().any(|event| {
        tag_value(event, "d").as_deref() == Some(IDENTIFIER)
            && tag_value(event, "refs/heads/main").as_deref() == Some(new_head.as_str())
    });
    assert!(
        !fanned_out_new_state,
        "failed git-server push should not publish the new state to remaining relays",
    );

    Ok(())
}
