//! An announcement must always name a way to reach the repository.
//!
//! A kind-30617 with an empty `relays` field carries no repository state and
//! no collaboration events; one with an empty `clone` field carries no git
//! data. `ngit init` and `ngit repo edit` both publish through the same
//! resolver, so the invariant is enforced once, on the resolved announcement
//! fields, before anything is signed. The malformed announcement that
//! prompted this (issue 3ef13932) came out of `ngit init`; the repository-edit
//! half is covered here because a metadata-only edit republishes hosting it
//! never looked at.
//!
//! Every refusal is asserted twice: the command exits non-zero **and** the
//! relay holds no announcement for the coordinate afterwards.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, tag_value};

const DISPLAY_NAME: &str = "hosting invariant";
const IDENTIFIER: &str = "hosting-invariant";

/// Every kind-30617 this author has on the harness's `default` relay.
async fn announcements(harness: &Harness, author: PublicKey) -> Result<Vec<Event>> {
    harness
        .relay("default")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await
}

/// Publish a fabricated event to the harness's `default` relay.
async fn publish_to_default_relay(harness: &Harness, event: &Event) -> Result<()> {
    let url = harness.relay("default").url().to_string();
    let client = Client::default();
    client.add_relay(&url).await?;
    client.connect().await;
    let output = client.send_event(event).to([url.as_str()]).await?;
    anyhow::ensure!(output.failed.is_empty(), "relay rejected event: {output:?}");
    Ok(())
}

/// No grasp server is registered, so ngit's default set is empty and
/// `--additional-clone` alone resolves to a git server with no relay to
/// announce it on. Before the invariant this published a relay-less
/// announcement — the shape reported in issue 3ef13932.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_refuses_an_announcement_without_a_relay() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("host")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let vanilla_url = harness.vanilla_git_server("host").url().to_string();

    let init = repo
        .ngit([
            "init",
            "--name",
            DISPLAY_NAME,
            "--identifier",
            IDENTIFIER,
            "--additional-clone",
            &vanilla_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init without any relay")?;
    assert!(
        !init.status.success(),
        "an announcement with no relay must be refused\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(
        stderr.contains("at least one relay"),
        "the refusal should say a relay is required: {stderr}",
    );
    assert!(
        stderr.contains("--grasp-server") && stderr.contains("--additional-relay"),
        "the refusal should name the flags that add a relay: {stderr}",
    );

    assert!(
        announcements(&harness, state.keys.public_key())
            .await?
            .is_empty(),
        "the refused init must not publish an announcement",
    );

    Ok(())
}

/// The grasp opt-out with an additional relay but no additional clone URL
/// leaves the `clone` field empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_refuses_an_announcement_without_a_git_server() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    // Registered so a grasp default is available: the refusal must come from
    // the resolved announcement, not from an empty default set.
    .with_grasp_server("repo")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let default_relay_url = harness.relay("default").url().to_string();

    let init = repo
        .ngit([
            "init",
            "--name",
            DISPLAY_NAME,
            "--identifier",
            IDENTIFIER,
            "--grasp-server",
            "",
            "--additional-relay",
            &default_relay_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init without any git server")?;
    assert!(
        !init.status.success(),
        "an announcement with no git server must be refused\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(
        stderr.contains("at least one git server"),
        "the refusal should say a git server is required: {stderr}",
    );
    assert!(
        stderr.contains("--grasp-server") && stderr.contains("--additional-clone"),
        "the refusal should name the flags that add a git server: {stderr}",
    );

    assert!(
        announcements(&harness, state.keys.public_key())
            .await?
            .is_empty(),
        "the refused init must not publish an announcement",
    );

    Ok(())
}

/// A metadata-only `ngit repo edit` republishes the announcement's hosting
/// verbatim, so an announcement that already lacks hosting has to be repaired
/// in the same command rather than re-signed unusable. The fixture fabricates
/// the hosting-less announcement directly — ngit itself will no longer
/// produce one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_only_edit_of_a_hosting_less_announcement_is_refused() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    // State B leaves `nostr.repo` pointing at a coordinate whose relay hint is
    // the default relay, which is all the announcement below needs to be
    // discoverable — it announces no relays of its own.
    let (repo, state) = harness.arrange_init_state_b_coordinate_only().await?;
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier(state.coordinate_identifier.clone()),
            Tag::custom("r", vec![state.root_oid.clone(), "euc".to_string()]),
            Tag::custom("name", vec!["hosting-less repository".to_string()]),
            Tag::custom("description", vec![String::new()]),
        ])
        .custom_created_at(Timestamp::now() - 30u64)
        .finalize(&state.keys)?;
    publish_to_default_relay(&harness, &announcement).await?;

    let edit = repo
        .ngit(["repo", "edit", "--name", "renamed"])
        .output()
        .await
        .context("failed to spawn ngit repo edit")?;
    assert!(
        !edit.status.success(),
        "a metadata-only edit must not republish an announcement with no \
         hosting\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&edit.stdout),
        String::from_utf8_lossy(&edit.stderr),
    );
    let stderr = String::from_utf8_lossy(&edit.stderr);
    assert!(
        stderr.contains("at least one relay and one git server"),
        "the refusal should say both fields are required: {stderr}",
    );
    assert!(
        stderr.contains("--add-grasp-server")
            && stderr.contains("--add-additional-relay")
            && stderr.contains("--add-additional-clone"),
        "repo edit's refusal should suggest its own targeted add actions: {stderr}",
    );

    let published = announcements(&harness, state.keys.public_key()).await?;
    let for_coordinate: Vec<&Event> = published
        .iter()
        .filter(|event| {
            tag_value(event, "d").as_deref() == Some(state.coordinate_identifier.as_str())
        })
        .collect();
    assert_eq!(
        for_coordinate.len(),
        1,
        "the refused edit must not publish an announcement; got {for_coordinate:?}",
    );
    assert_eq!(
        for_coordinate[0].id, announcement.id,
        "the fabricated announcement must be left untouched",
    );

    Ok(())
}
