//! Scenario: a vanilla git server is fast-forward ahead of nostr state
//! because a push bypassed nostr (e.g. a plain `git push` straight to
//! the server's URL). Plain `ngit sync` must neither adopt the server's
//! tip into nostr state nor downgrade the server back to the (older)
//! nostr state. `ngit sync -t` (`--trust-server`) must publish an
//! updated kind-30618 naming the server's tip and propagate the commits
//! to the repository's other git servers.
//!
//! A second case covers the `nostr.trust-server-domains` git-config
//! knob: with the server's domain listed, plain `ngit sync` auto-trusts
//! it without `-t`.

use anyhow::{Context, Result};
use test_harness::tag_value;

use crate::common::{
    SyncSetup, bare_ref, commit_new_file, latest_state_event, setup_vanilla, sync_ok,
};

/// Shared arrangement tail: one commit pushed straight to server-a,
/// bypassing nostr, leaving server-a strictly fast-forward ahead of
/// both the nostr state and server-b. Returns the ahead tip's oid.
async fn push_ahead_commit_to_first_server(setup: &SyncSetup) -> Result<String> {
    let publisher = &setup.publisher;
    let ahead_oid = commit_new_file(publisher, "ahead.md", "ahead commit").await?;
    publisher
        .git_ok(
            ["push", &setup.server_urls[0], "main"],
            "git push <server-a-url> main",
        )
        .await?;

    let bare_a = setup.harness.vanilla_git_server("server-a").repo_path();
    let bare_b = setup.harness.vanilla_git_server("server-b").repo_path();
    assert_eq!(
        bare_ref(bare_a, "refs/heads/main").await?,
        Some(ahead_oid.clone()),
        "arrange: server-a should hold the out-of-band commit",
    );
    assert_eq!(
        bare_ref(bare_b, "refs/heads/main").await?,
        Some(setup.main_oid.clone()),
        "arrange: server-b should still be at the nostr state",
    );
    Ok(ahead_oid)
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_server_adopts_ahead_server_and_propagates() -> Result<()> {
    let setup = setup_vanilla("sync-trust-server", &["server-a", "server-b"]).await?;
    let publisher = &setup.publisher;
    let maintainer = setup.maintainer_keys.public_key();

    let ahead_oid = push_ahead_commit_to_first_server(&setup).await?;
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

    let state_before = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        tag_value(&state_before, "refs/heads/main"),
        Some(setup.main_oid.clone()),
        "arrange: nostr state should still name the seed commit",
    );

    // ---- stage 1: plain sync — no adoption, no downgrade ---------------
    sync_ok(publisher, &[]).await?;

    let state_mid = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        state_mid.id, state_before.id,
        "plain sync must not update nostr state from an ahead server",
    );
    assert_eq!(
        bare_ref(&bare_a, "refs/heads/main").await?,
        Some(ahead_oid.clone()),
        "plain sync must not downgrade the ahead server to the older nostr state",
    );
    assert_eq!(
        bare_ref(&bare_b, "refs/heads/main").await?,
        Some(setup.main_oid.clone()),
        "plain sync must not propagate untrusted commits to other servers",
    );

    // ---- stage 2: -t adopts and propagates -----------------------------
    sync_ok(publisher, &["-t"]).await?;

    let state_after = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_ne!(
        state_after.id, state_before.id,
        "-t must publish an updated state event",
    );
    assert_eq!(
        tag_value(&state_after, "refs/heads/main"),
        Some(ahead_oid.clone()),
        "-t must adopt the ahead server's tip into nostr state",
    );
    assert_eq!(
        bare_ref(&bare_b, "refs/heads/main").await?,
        Some(ahead_oid.clone()),
        "-t must propagate the adopted commits to the other git server",
    );
    assert_eq!(
        bare_ref(&bare_a, "refs/heads/main").await?,
        Some(ahead_oid.clone()),
        "the ahead server itself must be left at its tip",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_server_domains_config_auto_adopts_without_flag() -> Result<()> {
    let setup = setup_vanilla("sync-trust-domains", &["server-a", "server-b"]).await?;
    let publisher = &setup.publisher;
    let maintainer = setup.maintainer_keys.public_key();

    // Both harness servers live on 127.0.0.1, so this trusts them both;
    // only server-a is ahead, so only its tip gets adopted.
    publisher
        .git_ok(
            [
                "config",
                "--local",
                "nostr.trust-server-domains",
                "127.0.0.1",
            ],
            "git config nostr.trust-server-domains",
        )
        .await?;

    let ahead_oid = push_ahead_commit_to_first_server(&setup).await?;
    let bare_b = setup
        .harness
        .vanilla_git_server("server-b")
        .repo_path()
        .to_path_buf();

    let state_before = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        tag_value(&state_before, "refs/heads/main"),
        Some(setup.main_oid.clone()),
        "arrange: nostr state should still name the seed commit",
    );

    // Plain sync — no -t — must auto-trust via the domain list.
    sync_ok(publisher, &[]).await?;

    let state_after = latest_state_event(&setup.harness, maintainer, &setup.identifier)
        .await
        .context("state event missing after domain-trusted sync")?;
    assert_ne!(
        state_after.id, state_before.id,
        "a domain-trusted sync must publish an updated state event without -t",
    );
    assert_eq!(
        tag_value(&state_after, "refs/heads/main"),
        Some(ahead_oid.clone()),
        "the domain-trusted server's tip must be adopted into nostr state",
    );
    assert_eq!(
        bare_ref(&bare_b, "refs/heads/main").await?,
        Some(ahead_oid.clone()),
        "the adopted commits must propagate to the other git server",
    );

    Ok(())
}
