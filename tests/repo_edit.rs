//! Normal-path coverage for named repository relationship edits.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishRepoOpts, tag_value, tag_values, tag_values_multiple};

async fn latest_announcement(
    harness: &Harness,
    author: PublicKey,
    identifier: &str,
) -> Result<Event> {
    harness
        .relay("default")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .context("repository announcement is missing")
}

async fn edit_ok(repo: &test_harness::Repo, args: &[&str]) -> Result<()> {
    let mut command = vec!["repo", "edit"];
    command.extend_from_slice(args);
    let output = repo.ngit(command).output().await?;
    if !output.status.success() {
        bail!(
            "ngit repo edit exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

#[tokio::test]
async fn named_add_and_remove_change_only_that_relationship() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("named roster edits".into()),
            identifier: Some("named-roster-edits".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob = Keys::generate().public_key();
    let bob_npub = bob.to_bech32()?;

    edit_ok(&publisher, &["--add-maintainer", &bob_npub]).await?;
    let invited = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        tag_values(&invited, "maintainers"),
        vec![alice.to_string(), bob.to_string()],
    );
    assert_eq!(tag_values_multiple(&invited, "M"), vec![alice.to_string()]);
    let active_m: Vec<String> = invited
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) == Some("m") && tag.len() % 2 == 1)
        .filter_map(|tag| tag.get(1).cloned())
        .collect();
    assert_eq!(active_m, vec![bob.to_string()]);

    let refused = publisher
        .ngit([
            "repo",
            "edit",
            "--remove-maintainer",
            &bob_npub,
            "--no-lead-maintainer",
        ])
        .output()
        .await?;
    assert!(
        !refused.status.success(),
        "--no-lead-maintainer must not erase an existing lead",
    );
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        invited.id,
        "a refused governance change must not publish an announcement",
    );

    edit_ok(&publisher, &["--remove-maintainer", &bob_npub]).await?;
    let removed = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(tag_values(&removed, "maintainers"), vec![alice.to_string()]);
    assert_eq!(tag_values_multiple(&removed, "M"), vec![alice.to_string()]);
    let bob_history: Vec<Vec<String>> = removed
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .filter(|tag| {
            tag.first().map(String::as_str) == Some("m") && tag.get(1) == Some(&bob.to_string())
        })
        .collect();
    assert_eq!(bob_history.len(), 1);
    assert_eq!(bob_history[0].len(), 4, "Bob's invitation should be ended");
    assert!(
        bob_history[0][2].parse::<u64>().is_ok() && bob_history[0][3].parse::<u64>().is_ok(),
        "Bob's invitation history should retain numeric start/end boundaries",
    );
    Ok(())
}
