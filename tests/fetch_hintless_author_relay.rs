//! A hint-less repository coordinate follows its author's NIP-65 write relays.
//!
//! The bootstrap relay contains only the author's kind-10002 relay list. The
//! matching kind-30617 announcement exists solely on that advertised write
//! relay, so a successful clone proves discovery expands beyond indexers.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishRepoOpts};

#[tokio::test]
async fn hintless_clone_finds_announcement_on_author_write_relay() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("author")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("hintless author relay".into()),
            identifier: Some("hintless-author-relay-source".into()),
            ..Default::default()
        })
        .await?;

    let original = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?
        .into_iter()
        .find(|event| tag_value(event, "d") == Some(published.identifier.as_str()))
        .context("source repository announcement was not queryable on the grasp")?;

    // Reuse the proven git hosting and repository relay from the source
    // announcement, but publish a distinct coordinate only on the author's
    // advertised write relay. The Git server need not share the coordinate's
    // author or identifier; clone URLs are deliberately server-agnostic.
    let author_keys = Keys::generate();
    let identifier = "hintless-author-relay";
    let mut tags = original
        .tags
        .iter()
        .filter(|tag| !matches!(tag.as_slice(), [name, ..] if name == "d" || name == "maintainers"))
        .cloned()
        .collect::<Vec<_>>();
    tags.push(Tag::identifier(identifier));
    tags.push(Tag::custom(
        "maintainers",
        vec![author_keys.public_key().to_string()],
    ));
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(tags)
        .finalize(&author_keys)?;

    let author_relay = harness.relay("author").url();
    publish_to_relay(author_relay, &announcement).await?;
    let relay_list = EventBuilder::new(Kind::RelayList, "")
        .tag(Tag::parse(["r", author_relay, "write"])?)
        .finalize(&author_keys)?;
    publish_to_relay(harness.relay("default").url(), &relay_list).await?;

    let indexed_announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(author_keys.public_key())
                .kind(Kind::GitRepoAnnouncement)
                .identifier(identifier),
        )
        .await?;
    assert!(
        indexed_announcements.is_empty(),
        "the bootstrap indexer must not contain the target announcement"
    );

    let npub = author_keys.public_key().to_bech32()?;
    let clone = harness
        .clone_url(&format!("nostr://{npub}/{identifier}"))
        .await
        .context("clone hint-less coordinate through the author write relay")?;
    let snapshot = clone.snapshot()?;
    assert_eq!(
        snapshot.refs.get("refs/heads/main"),
        Some(&published.initial_oid),
        "clone must reproduce the source repository tip"
    );
    assert_eq!(
        std::fs::read_to_string(clone.dir().join("README.md"))?,
        "hello, ngit!\n"
    );

    Ok(())
}

async fn publish_to_relay(relay_url: &str, event: &Event) -> Result<()> {
    let client = Client::default();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("add relay {relay_url}"))?;
    client.connect().await;
    let output = client
        .send_event(event)
        .to([relay_url])
        .await
        .with_context(|| format!("publish event {} to {relay_url}", event.id))?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "relay {relay_url} rejected event {}: {:?}",
            event.id,
            output.failed
        );
    }
    Ok(())
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [tag_name, value, ..] if tag_name == name => Some(value.as_str()),
        _ => None,
    })
}
