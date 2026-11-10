//! Optional metadata warming belongs to the full repository fetch report.
use anyhow::{Context, Result, ensure};
use nostr_sdk::prelude::*;
use test_harness::{GraspObservation, GraspRecorder, GraspRelayMode, Harness, PublishRepoOpts};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_repo_fetch_does_not_wait_for_stalled_nip11() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let recorder =
        GraspRecorder::start_with_mode(harness.grasp("repo"), GraspRelayMode::StallNip11).await?;
    let relay = recorder.url().replacen("http://", "ws://", 1);
    let announcements = harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitRepoAnnouncement)
                .author(published.maintainer_keys.public_key()),
        )
        .await?;
    let original = announcements
        .iter()
        .max_by_key(|event| event.created_at)
        .context("missing repository announcement")?;
    let mut tags: Vec<_> = original
        .tags
        .iter()
        .filter(|tag| tag.kind() != "relays")
        .cloned()
        .collect();
    tags.push(Tag::parse(["relays", &relay])?);
    let announcement = EventBuilder::new(original.kind, original.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(original.created_at.as_secs() + 1))
        .finalize(&published.maintainer_keys)?;
    ngit::client::save_event_in_local_cache(publisher.dir(), &announcement).await?;
    let cache = tempfile::tempdir()?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        publisher
            .ngit(["sync"])
            .env("NGIT_CACHE_DIR", cache.path())
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !recorder
            .observations()
            .contains(&GraspObservation::Nip11Cancelled)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("metadata was not cancelled at fetch completion")?;
    let observations = recorder.observations();
    let query = observations
        .iter()
        .position(|entry| *entry == GraspObservation::EventRequest)
        .context("no Nostr request")?;
    let cancelled = observations
        .iter()
        .position(|entry| *entry == GraspObservation::Nip11Cancelled)
        .context("no metadata cancellation")?;
    assert!(
        query < cancelled,
        "event fetching waited for metadata: {observations:?}"
    );
    assert_eq!(
        observations
            .iter()
            .filter(|entry| **entry == GraspObservation::Nip11Requested)
            .count(),
        1,
        "one fetch must not duplicate or retry its metadata request"
    );
    Ok(())
}
