//! End-to-end coverage for a stale force-with-lease on repository state.
//!
//! Two maintainer checkouts start from the same `main`. The publisher advances
//! and pushes `main`, replacing the kind-30618 state event. The stale checkout
//! then creates a divergent commit and attempts a guarded force push using its
//! old `origin/main` as the lease. The push must fail, and neither the Nostr
//! state nor a subsequent clone may observe the rejected commit. After fetching
//! the winning tip, the same guarded force push must succeed, exercising the
//! remote helper's matching `cas` path as well as Git's stale-lease rejection.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{CloneLogin, Harness, KIND_REPO_STATE, PublishRepoOpts, tag_value};

const IDENTIFIER: &str = "state-push-force-with-lease";

#[tokio::test]
async fn stale_lease_rejects_before_matching_lease_updates_state() -> Result<()> {
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
            display_name: Some("force-with-lease maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;
    let stale_checkout = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    std::fs::write(
        publisher.dir().join("winner.md"),
        "published state update\n",
    )
    .context("failed to write publisher update")?;
    publisher
        .git_ok(["add", "winner.md"], "git add publisher update")
        .await?;
    publisher
        .git_ok(
            [
                "commit",
                "-m",
                "publish winning state update",
                "--no-gpg-sign",
            ],
            "git commit publisher update",
        )
        .await?;
    let published_tip = publisher.rev_parse("HEAD").await?;
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("publisher failed to advance repository state")?;

    std::fs::write(
        stale_checkout.dir().join("stale.md"),
        "conflicting stale update\n",
    )
    .context("failed to write stale update")?;
    stale_checkout
        .git_ok(["add", "stale.md"], "git add stale update")
        .await?;
    stale_checkout
        .git_ok(
            [
                "commit",
                "-m",
                "create conflicting stale update",
                "--no-gpg-sign",
            ],
            "git commit stale update",
        )
        .await?;
    let rejected_tip = stale_checkout.rev_parse("HEAD").await?;

    let rejected = stale_checkout
        .nostr_push(["--force-with-lease", "origin", "main"])
        .await;
    assert!(
        rejected.is_err(),
        "stale force-with-lease unexpectedly succeeded"
    );

    let state_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(KIND_REPO_STATE),
        )
        .await?;
    let current_state = state_events
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(IDENTIFIER))
        .context("current repository state event missing after rejected push")?;
    assert_eq!(
        tag_value(current_state, "refs/heads/main").as_deref(),
        Some(published_tip.as_str()),
        "rejected lease changed the published main ref",
    );
    assert_ne!(
        tag_value(current_state, "refs/heads/main").as_deref(),
        Some(rejected_tip.as_str()),
        "rejected commit appeared in the repository state",
    );

    let fresh_clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    assert_eq!(
        fresh_clone.rev_parse("HEAD").await?,
        published_tip,
        "fresh clone observed the rejected state update",
    );

    stale_checkout
        .git_ok(["fetch", "origin"], "git fetch origin after stale lease")
        .await?;
    assert_eq!(
        stale_checkout.rev_parse("refs/remotes/origin/main").await?,
        published_tip,
        "fetch did not advance the lease to the published winner",
    );
    stale_checkout
        .nostr_push(["--force-with-lease", "origin", "main"])
        .await
        .context("matching force-with-lease failed after refreshing origin/main")?;

    let updated_state_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(KIND_REPO_STATE),
        )
        .await?;
    assert!(
        updated_state_events.iter().any(|event| {
            tag_value(event, "d").as_deref() == Some(IDENTIFIER)
                && tag_value(event, "refs/heads/main").as_deref() == Some(rejected_tip.as_str())
        }),
        "matching lease did not publish the guarded state update",
    );

    Ok(())
}
