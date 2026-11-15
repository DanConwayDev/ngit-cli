//! Deletions must converge in one sync, including when the original and its
//! deletion are served by different repository relays.
use anyhow::{Context, Result};
use ngit::client::{get_active_label_events, get_events_from_local_cache};
use nostr_sdk::prelude::*;
use test_harness::Harness;

async fn publish(url: &str, events: &[Event]) -> Result<()> {
    let client = Client::default();
    client.add_relay(url).await?;
    client.connect().await;
    for event in events {
        let sent = client.send_event(event).await?;
        assert!(
            !sent.success.is_empty(),
            "relay rejected fixture: {:?}",
            sent.failed
        );
    }
    client.disconnect().await;
    Ok(())
}

async fn sync(repo: &test_harness::Repo, coordinate: &str) -> Result<()> {
    let output = repo
        .ngit(["--repo", coordinate, "issue", "list", "--json"])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn label_discovery_converges_across_relays_without_probing_other_kinds() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("other")
    .build()
    .await?;
    let keys = Keys::generate();
    let coordinate =
        Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key()).identifier("deletions");
    let naddr = Nip19Coordinate::new(
        coordinate.clone(),
        [RelayUrl::parse(harness.relay("default").url())?],
    )
    .to_bech32()?;
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier("deletions"),
            Tag::parse([
                "relays",
                harness.relay("default").url(),
                harness.relay("other").url(),
            ])?,
            Tag::parse(["maintainers", &keys.public_key().to_hex()])?,
        ])
        .finalize(&keys)?;
    let issue = EventBuilder::new(Kind::GitIssue, "body")
        .tags([
            Tag::parse(["a", &coordinate.to_string()])?,
            Tag::parse(["subject", "deletion fixture"])?,
        ])
        .finalize(&keys)?;
    let mut targets = Vec::new();
    for kind in [1111, 1624, 1630, 1985] {
        targets.push(
            EventBuilder::new(Kind::from(kind), "descendant")
                .tags([
                    Tag::event(issue.id),
                    Tag::parse(["E", &issue.id.to_hex()])?,
                    Tag::parse(["K", "1621"])?,
                    Tag::parse(["P", &keys.public_key().to_hex()])?,
                    Tag::parse(["L", "#t"])?,
                    Tag::parse(["l", "bug", "#t"])?,
                ])
                .finalize(&keys)?,
        );
    }
    publish(
        harness.relay("default").url(),
        &[vec![announcement.clone(), issue.clone()], targets.clone()].concat(),
    )
    .await?;
    publish(harness.relay("other").url(), &[announcement, issue]).await?;
    let warm = harness.fresh_repo()?;
    sync(&warm, &naddr).await?;
    for target in &targets {
        assert_eq!(
            get_events_from_local_cache(warm.dir(), vec![Filter::new().id(target.id)])
                .await?
                .len(),
            1,
            "kind {} was not fetched",
            target.kind
        );
    }
    let mut deletions = Vec::new();
    for target in &targets {
        deletions.push(
            EventBuilder::new(Kind::EventDeletion, "removed elsewhere")
                .tag(Tag::event(target.id))
                .finalize(&keys)?,
        );
    }
    publish(harness.relay("other").url(), &deletions).await?;
    let cold = harness.fresh_repo()?;
    for repo in [&warm, &cold] {
        sync(repo, &naddr).await?;
        for target in &targets {
            let cached =
                get_events_from_local_cache(repo.dir(), vec![Filter::new().id(target.id)]).await?;
            assert_eq!(
                cached.is_empty(),
                target.kind == Kind::Custom(1985),
                "unexpected deletion discovery for kind {}",
                target.kind
            );
        }
        // Relay A still serves every original. Another refresh must not
        // resurrect the deleted label or broaden discovery to other kinds.
        sync(repo, &naddr).await.context("repeat sync")?;
        assert!(
            get_active_label_events(repo.dir(), vec![Filter::new().kind(Kind::Custom(1985))])
                .await?
                .is_empty()
        );
    }
    Ok(())
}
