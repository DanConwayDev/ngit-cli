//! Scenario: nostr state is ahead of one vanilla git server, e.g.
//! because the server missed a push (simulated here by rolling its
//! `main` back out-of-band). Plain `ngit sync` must bring the stale
//! server back up to the nostr state with a plain fast-forward push and
//! must not publish a new state event — nothing about the canonical
//! state changed.

use anyhow::{Context, Result};
use test_harness::tag_value;

use crate::common::{
    bare_ref, commit_new_file, latest_state_event, set_bare_ref, setup_vanilla, sync_ok,
};

#[tokio::test(flavor = "multi_thread")]
async fn fast_forwards_stale_server_without_republishing_state() -> Result<()> {
    let setup = setup_vanilla("sync-ff-propagation", &["server-a", "server-b"]).await?;
    let publisher = &setup.publisher;
    let maintainer = setup.maintainer_keys.public_key();

    // Advance `main` through the nostr remote: the state event and both
    // servers move to the new tip.
    let second_oid = commit_new_file(publisher, "second.md", "second commit").await?;
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main (second commit)")?;

    let bare_a = setup
        .harness
        .vanilla_git_server("server-a")
        .repo_path()
        .to_path_buf();
    let bare_b = setup
        .harness
        .vanilla_git_server("server-b")
        .repo_path()
        .to_path_buf();
    assert_eq!(
        bare_ref(&bare_a, "refs/heads/main").await?,
        Some(second_oid.clone()),
        "arrange: the nostr push should have updated server-a",
    );
    assert_eq!(
        bare_ref(&bare_b, "refs/heads/main").await?,
        Some(second_oid.clone()),
        "arrange: the nostr push should have updated server-b",
    );

    // Out-of-band rollback: server-b now behaves as if it missed the
    // second push, leaving nostr state one commit ahead of it.
    set_bare_ref(&bare_b, "refs/heads/main", &setup.main_oid).await?;

    let state_before = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        tag_value(&state_before, "refs/heads/main"),
        Some(second_oid.clone()),
        "arrange: nostr state should already name the second commit",
    );

    sync_ok(publisher, &[]).await?;

    assert_eq!(
        bare_ref(&bare_b, "refs/heads/main").await?,
        Some(second_oid.clone()),
        "sync should fast-forward the stale server up to the nostr state",
    );
    assert_eq!(
        bare_ref(&bare_a, "refs/heads/main").await?,
        Some(second_oid.clone()),
        "the already-in-sync server must be left as-is",
    );

    let state_after = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        state_after.id, state_before.id,
        "propagating existing state to a behind server must not publish a new state event",
    );

    Ok(())
}
