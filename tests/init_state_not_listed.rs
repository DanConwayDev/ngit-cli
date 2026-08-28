//! `ngit init` refuses to join or replace somebody else's repository.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, tag_value};

#[tokio::test]
async fn unrelated_user_cannot_force_init_into_an_existing_repository() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (repo, state) = harness.arrange_init_state_e_not_listed().await?;
    let grasp_url = harness.grasp("repo").url().to_string();

    let output = repo
        .ngit([
            "init",
            "--force",
            "--defaults",
            "--grasp-server",
            &grasp_url,
        ])
        .output()
        .await
        .context("failed to spawn forced ngit init as an unrelated user")?;

    assert!(
        !output.status.success(),
        "init must not join another repository\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot join or replace"),
        "the refusal should explain the init boundary: {stderr}",
    );

    let authored = harness
        .relay("default")
        .events(
            Filter::new()
                .author(state.keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    assert!(
        authored
            .iter()
            .all(|event| tag_value(event, "d").as_deref()
                != Some(state.coordinate_identifier.as_str())),
        "a refused forced init must not publish an announcement",
    );
    Ok(())
}
