//! Scenario: a vanilla (non-grasp) git server holds refs that would
//! require a rewrite or a deletion to match nostr state — a diverged
//! `main` (force-pushed to the server out-of-band) and a stray branch
//! nostr state knows nothing about.
//!
//! Plain `ngit sync` follows a fast-forward-only policy on non-grasp
//! servers: it must leave both refs untouched and still exit
//! successfully. `ngit sync --force` then rewrites the diverged ref to
//! the nostr state, deletes the stray branch, and republishes the state
//! event (--force always re-signs and broadcasts a fresh event, even
//! when the ref map is unchanged — that is its repair semantic).

use anyhow::{Context, Result};
use test_harness::tag_value;

use crate::common::{bare_ref, commit_new_file, latest_state_event, setup_vanilla, sync_ok};

#[tokio::test(flavor = "multi_thread")]
async fn plain_sync_leaves_diverged_refs_then_force_rewrites_and_deletes() -> Result<()> {
    let setup = setup_vanilla("sync-force-divergence", &["server"]).await?;
    let publisher = &setup.publisher;
    let maintainer = setup.maintainer_keys.public_key();
    let server_url = setup.server_urls[0].clone();
    let bare = setup
        .harness
        .vanilla_git_server("server")
        .repo_path()
        .to_path_buf();

    // Advance `main` through the nostr remote so nostr state and the
    // server share the history seed -> second.
    let second_oid = commit_new_file(publisher, "second.md", "second commit").await?;
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main (second commit)")?;

    // Out-of-band stray branch: exists on the server, unknown to nostr
    // state.
    publisher
        .git_ok(
            [
                "push",
                &server_url,
                &format!("{second_oid}:refs/heads/stray"),
            ],
            "git push <server-url> stray",
        )
        .await?;

    // Out-of-band divergence: a commit on top of the seed commit,
    // force-pushed straight to the server's `main`. The server and
    // nostr state now each have a commit the other lacks.
    publisher
        .git_ok(
            ["checkout", "--detach", &setup.main_oid],
            "git checkout --detach seed",
        )
        .await?;
    let divergent_oid = commit_new_file(publisher, "divergent.md", "divergent commit").await?;
    publisher
        .git_ok(
            ["push", "--force", &server_url, "HEAD:refs/heads/main"],
            "git push --force <server-url> main",
        )
        .await?;
    publisher
        .git_ok(["checkout", "main"], "git checkout main")
        .await?;

    let state_before = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        tag_value(&state_before, "refs/heads/main"),
        Some(second_oid.clone()),
        "arrange: nostr state should name the second commit",
    );

    // ---- stage 1: plain sync — fast-forward-only ----------------------
    sync_ok(publisher, &[]).await?;

    assert_eq!(
        bare_ref(&bare, "refs/heads/main").await?,
        Some(divergent_oid.clone()),
        "plain sync must not rewrite a diverged ref on a non-grasp server",
    );
    assert_eq!(
        bare_ref(&bare, "refs/heads/stray").await?,
        Some(second_oid.clone()),
        "plain sync must not delete refs from a non-grasp server",
    );
    let state_mid = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_eq!(
        state_mid.id, state_before.id,
        "plain sync of a diverged server must not touch the state event",
    );

    // ---- stage 2: --force ---------------------------------------------
    sync_ok(publisher, &["--force"]).await?;

    assert_eq!(
        bare_ref(&bare, "refs/heads/main").await?,
        Some(second_oid.clone()),
        "--force must rewrite the diverged ref back to the nostr state",
    );
    assert_eq!(
        bare_ref(&bare, "refs/heads/stray").await?,
        None,
        "--force must delete refs that are absent from nostr state",
    );

    let state_after = latest_state_event(&setup.harness, maintainer, &setup.identifier).await?;
    assert_ne!(
        state_after.id, state_before.id,
        "--force republishes a fresh state event even when the ref map is unchanged",
    );
    assert_eq!(
        tag_value(&state_after, "refs/heads/main"),
        Some(second_oid.clone()),
        "the republished state event must carry the same refs",
    );

    Ok(())
}
