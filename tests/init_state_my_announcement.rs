//! `ngit init` refuses to edit an existing repository announcement.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, tag_value};

#[tokio::test]
async fn existing_author_is_directed_to_repo_edit_without_publication() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    let (repo, state) = harness.arrange_init_state_c_my_announcement().await?;

    for args in [
        vec!["init"],
        vec!["init", "--force"],
        vec!["init", "--name", "replacement name"],
        vec!["init", "--identifier", "different-identifier", "--force"],
    ] {
        let output = repo
            .ngit(args.clone())
            .output()
            .await
            .with_context(|| format!("failed to spawn {}", args.join(" ")))?;
        assert!(
            !output.status.success(),
            "{} must not edit an existing announcement\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("ngit repo edit"),
            "the refusal should direct the author to repo edit: {stderr}",
        );
    }

    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(state.keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let winner = announcements
        .into_iter()
        .filter(|event| {
            tag_value(event, "d").as_deref() == Some(state.coordinate_identifier.as_str())
        })
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .context("existing announcement disappeared")?;
    assert_eq!(
        winner.id, state.existing_announcement.id,
        "refused init invocations must not replace the existing announcement",
    );
    Ok(())
}
