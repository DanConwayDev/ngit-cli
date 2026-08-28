//! A pushed `maintainers.yaml` is repository content, not a membership API.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishRepoOpts, tag_value};

#[tokio::test]
async fn pushed_maintainers_yaml_does_not_replace_the_announcement() -> Result<()> {
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
            display_name: Some("maintainers yaml is content".into()),
            identifier: Some("maintainers-yaml-is-content".into()),
            ..Default::default()
        })
        .await?;

    let author = published.maintainer_keys.public_key();
    let announcement_before = harness
        .grasp("repo")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("published repository announcement is missing")?;

    let unrelated = Keys::generate().public_key().to_bech32()?;
    std::fs::write(
        publisher.dir().join("maintainers.yaml"),
        format!(
            "identifier: {}\nmaintainers:\n  - {}\nrelays:\n  - wss://unrelated.example\n",
            published.identifier, unrelated
        ),
    )
    .context("write maintainers.yaml")?;
    publisher
        .git_ok(["add", "maintainers.yaml"], "git add maintainers.yaml")
        .await?;
    publisher
        .git_ok(
            [
                "commit",
                "-m",
                "add legacy coordinate file",
                "--no-gpg-sign",
            ],
            "git commit maintainers.yaml",
        )
        .await?;
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("push maintainers.yaml")?;

    let announcement_after = harness
        .grasp("repo")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("repository announcement disappeared after push")?;

    assert_eq!(
        announcement_after.id, announcement_before.id,
        "pushing maintainers.yaml must not publish a replacement announcement",
    );
    Ok(())
}
