//! `ngit init` refuses to accept an invitation to an existing repository.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, tag_value};

#[tokio::test]
async fn invited_user_is_directed_to_repo_accept_without_publication() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (repo, state) = harness.arrange_init_state_d_co_maintainer().await?;
    let grasp_url = harness.grasp("repo").url().to_string();

    let output = repo
        .ngit(["init", "--force", "--grasp-server", &grasp_url])
        .output()
        .await
        .context("failed to spawn ngit init as an invitee")?;

    assert!(
        !output.status.success(),
        "init must not accept an invitation\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ngit repo accept"),
        "the refusal should direct the invitee to repo accept: {stderr}",
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
        "a refused init must not publish an acceptance announcement",
    );
    Ok(())
}
