//! Scenario: a GRASP server is added to the repository announcement
//! after the repo already has nostr state, so the grasp's relay lacks
//! the current kind-30618 state event and its bare repo has no refs.
//! `ngit sync` must seed it: publish the *existing* state event to the
//! grasp relay first (grasp rejects git pushes until the state event is
//! present), then push the git data over smart-http.
//!
//! Arrangement: announce with only a vanilla git server and push `main`
//! (state event lands on the default relay). Then publish a strictly
//! newer announcement revision that adds the grasp's clone URL and
//! relay, and run `ngit sync`.

use std::time::Duration;

use anyhow::Result;
use nostr_sdk::prelude::*;
use test_harness::{Harness, KIND_REPO_STATE, tag_value};

use crate::common::{
    announce_and_push, bare_ref, latest_state_event, publish_event_to_all, sign_announcement,
    sync_ok, wait_for_path,
};

#[tokio::test(flavor = "multi_thread")]
async fn seeds_state_event_and_git_data_to_new_grasp_server() -> Result<()> {
    let identifier = "sync-grasp-seeding";
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("server")
    .with_grasp_server("repo")
    .build()
    .await?;

    let vanilla_url = format!(
        "{}/{identifier}.git",
        harness.vanilla_git_server("server").url()
    );
    let setup = announce_and_push(harness, identifier, vec![vanilla_url.clone()]).await?;
    let maintainer = setup.maintainer_keys.public_key();

    let state_event = latest_state_event(&setup.harness, maintainer, identifier).await?;
    assert_eq!(
        tag_value(&state_event, "refs/heads/main"),
        Some(setup.main_oid.clone()),
        "arrange: state event on the default relay should name the seed commit",
    );

    let grasp = setup.harness.grasp("repo");
    let grasp_clone_url = format!("{}/{}/{identifier}.git", grasp.url(), setup.npub);
    let grasp_relay_url = grasp.relay_url();

    // Sanity: the grasp relay has no state event before sync runs.
    let pre_sync = grasp
        .events(Filter::new().author(maintainer).kind(KIND_REPO_STATE))
        .await?;
    assert!(
        pre_sync.is_empty(),
        "arrange: grasp relay should not hold a state event yet, got {pre_sync:?}",
    );

    // Announcement v2: same repo, now also listing the grasp server.
    let v2 = sign_announcement(
        &setup.maintainer_keys,
        identifier,
        &setup.main_oid,
        &[vanilla_url.clone(), grasp_clone_url.clone()],
        &[setup.relay_url.clone(), grasp_relay_url.clone()],
        Some(&setup.announcement),
    )?;
    publish_event_to_all(&v2, &[setup.relay_url.as_str(), grasp_relay_url.as_str()]).await?;

    // The grasp creates the bare repo on announcement receipt; no push
    // completion barrier exists for a raw publish, so a bounded wait is
    // the sanctioned synchronisation here.
    let bare = grasp
        .git_data_path()
        .join(&setup.npub)
        .join(format!("{identifier}.git"));
    wait_for_path(&bare, Duration::from_secs(5)).await?;

    sync_ok(&setup.publisher, &[]).await?;

    // The grasp relay now holds the canonical state event — the very
    // same event, not a re-signed copy.
    let on_grasp: Vec<Event> = grasp
        .events(Filter::new().author(maintainer).kind(KIND_REPO_STATE))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .collect();
    assert!(
        on_grasp.iter().any(|event| event.id == state_event.id),
        "sync should publish the existing state event to the grasp relay; found {on_grasp:?}",
    );

    // The git data reached the grasp's bare repo.
    assert_eq!(
        bare_ref(&bare, "refs/heads/main").await?,
        Some(setup.main_oid.clone()),
        "sync should push the git data to the newly announced grasp server",
    );

    // The canonical state on the default relay is untouched.
    let state_after = latest_state_event(&setup.harness, maintainer, identifier).await?;
    assert_eq!(
        state_after.id, state_event.id,
        "seeding a grasp server must not publish a new state event",
    );

    Ok(())
}
