//! Moderator authorization boundaries, end-to-end against a fabricated
//! role graph (`Harness::publish_repo_with_role_graph`).
//!
//! Per NIP-34 "all members can perform other maintainer actions" — so a
//! confirmed (acknowledged) moderator may publish status and label events —
//! but only maintainers publish authoritative repository state. These tests
//! pin both sides of that line through the real CLI and remote-helper
//! paths:
//!
//! - an acknowledged moderator's `ngit pr label` / `ngit pr close` succeed and
//!   land signed events on the repository relays, and proposals tag the
//!   moderator's announcement coordinate;
//! - an assigned-but-unacknowledged moderator is refused by the publishing
//!   guard (`pr_status.rs`) before anything is signed;
//! - a moderator pushing a protected branch through `git-remote-nostr` is
//!   rejected (`push.rs`'s maintainer-listing check), so no kind-30618 state
//!   event they signed ever exists.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    Harness, KIND_PULL_REQUEST, KIND_REPO_STATE, PublishPrOpts, tag_values_multiple,
};

/// NIP-32 label events (`src/bin/ngit/sub_commands/label.rs`).
const KIND_LABEL: Kind = Kind::Custom(1985);

#[tokio::test]
async fn acknowledged_moderator_can_label_and_set_status() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_lead_repo, graph) = harness
        .publish_repo_with_role_graph("moderator-member-actions")
        .await?;
    let moderator_pubkey = graph.moderator_keys.public_key();

    let pr = harness
        .publish_pr(
            &graph.published,
            PublishPrOpts {
                branch: None,
                commits: vec![],
                title: "a proposal for the moderator to act on".to_string(),
                description: "moderator authorization regression".to_string(),
                in_reply_to: vec![],
            },
        )
        .await?;

    // Proposals tag every current member's announcement coordinate —
    // including the confirmed moderator's — so clients subscribed via the
    // moderator's coordinate see the proposals that member can act on.
    let moderator_coordinate = format!(
        "30617:{}:{}",
        moderator_pubkey.to_hex(),
        graph.published.identifier,
    );
    let a_tags = tag_values_multiple(&pr.root_event, "a");
    assert!(
        a_tags.contains(&moderator_coordinate),
        "the PR's a tags should include the moderator's coordinate; got {a_tags:?}",
    );

    let moderator_clone = harness
        .clone_published_repo_as(&graph.published, &graph.moderator_keys)
        .await?;
    let pr_id = pr.event_id.to_hex();

    let label = moderator_clone
        .ngit(["pr", "label", &pr_id, "--label", "bug"])
        .output()
        .await
        .context("failed to spawn ngit pr label as moderator")?;
    assert!(
        label.status.success(),
        "ngit pr label as an acknowledged moderator exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        label.status,
        String::from_utf8_lossy(&label.stdout),
        String::from_utf8_lossy(&label.stderr),
    );
    let label_events = harness
        .relay("default")
        .events(Filter::new().author(moderator_pubkey).kind(KIND_LABEL))
        .await?;
    assert!(
        label_events
            .iter()
            .any(|e| tag_values_multiple(e, "e").contains(&pr_id)),
        "expected a moderator-signed kind-1985 label event referencing the PR; got {label_events:?}",
    );

    let close = moderator_clone
        .ngit(["pr", "close", &pr_id, "--reason", "closed by moderator"])
        .output()
        .await
        .context("failed to spawn ngit pr close as moderator")?;
    assert!(
        close.status.success(),
        "ngit pr close as an acknowledged moderator exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        close.status,
        String::from_utf8_lossy(&close.stdout),
        String::from_utf8_lossy(&close.stderr),
    );
    let status_events = harness
        .relay("default")
        .events(
            Filter::new()
                .author(moderator_pubkey)
                .kind(Kind::GitStatusClosed),
        )
        .await?;
    assert!(
        status_events
            .iter()
            .any(|e| tag_values_multiple(e, "e").contains(&pr_id)),
        "expected a moderator-signed closed-status event referencing the PR; got {status_events:?}",
    );

    Ok(())
}

#[tokio::test]
async fn unacknowledged_moderator_cannot_set_status() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_lead_repo, graph) = harness
        .publish_repo_with_role_graph("moderator-unacknowledged")
        .await?;
    let unack_pubkey = graph.unacknowledged_moderator_keys.public_key();

    let pr = harness
        .publish_pr(
            &graph.published,
            PublishPrOpts {
                branch: None,
                commits: vec![],
                title: "a proposal the invited moderator may not close".to_string(),
                description: "moderator authorization regression".to_string(),
                in_reply_to: vec![],
            },
        )
        .await?;

    let unack_clone = harness
        .clone_published_repo_as(&graph.published, &graph.unacknowledged_moderator_keys)
        .await?;
    let pr_id = pr.event_id.to_hex();

    let close = unack_clone
        .ngit(["pr", "close", &pr_id, "--reason", "not my call"])
        .output()
        .await
        .context("failed to spawn ngit pr close as unacknowledged moderator")?;
    assert!(
        !close.status.success(),
        "an assigned-but-unacknowledged moderator must not be able to change status\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&close.stdout),
        String::from_utf8_lossy(&close.stderr),
    );
    let stderr = String::from_utf8_lossy(&close.stderr).to_lowercase();
    assert!(
        stderr.contains("repository member"),
        "expected the member-authorization guard's error, got: {stderr}",
    );

    let status_events = harness
        .relay("default")
        .events(Filter::new().author(unack_pubkey).kinds(vec![
            Kind::GitStatusOpen,
            Kind::GitStatusApplied,
            Kind::GitStatusClosed,
            Kind::GitStatusDraft,
        ]))
        .await?;
    assert!(
        status_events.is_empty(),
        "the refused status change must not have published anything: {status_events:?}",
    );

    Ok(())
}

#[tokio::test]
async fn moderator_cannot_push_state() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_lead_repo, graph) = harness
        .publish_repo_with_role_graph("moderator-push-state")
        .await?;
    let moderator_pubkey = graph.moderator_keys.public_key();

    let moderator_clone = harness
        .clone_published_repo_as(&graph.published, &graph.moderator_keys)
        .await?;
    std::fs::write(
        moderator_clone.dir().join("moderator.md"),
        "a change the moderator may not land directly\n",
    )?;
    moderator_clone
        .git_ok(["add", "moderator.md"], "git add moderator.md")
        .await?;
    moderator_clone
        .git_ok(
            ["commit", "-m", "moderator commit", "--no-gpg-sign"],
            "git commit (moderator)",
        )
        .await?;

    let out = moderator_clone
        .nostr_push_expecting_failure(["origin", "main"])
        .await?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .to_lowercase();
    assert!(
        combined.contains("maintainer"),
        "the rejected push should cite the maintainer-listing check, got: {combined}",
    );

    let state_events = harness
        .grasp("repo")
        .events(Filter::new().author(moderator_pubkey).kind(KIND_REPO_STATE))
        .await?;
    assert!(
        state_events.is_empty(),
        "a moderator must never produce an authoritative state event: {state_events:?}",
    );

    Ok(())
}

/// A moderator pushing a protected branch *alongside* a proposal branch:
/// the branch refspec is rejected by the maintainer-listing check — git
/// exits non-zero, no state event exists and the rejected refspec must not
/// re-enter the state transaction or be reported to git a second time —
/// while the `pr/` refspec still lands its proposal events.
#[tokio::test]
async fn moderator_mixed_push_rejects_branch_but_delivers_proposal() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_lead_repo, graph) = harness
        .publish_repo_with_role_graph("moderator-mixed-push")
        .await?;
    let moderator_pubkey = graph.moderator_keys.public_key();

    let moderator_clone = harness
        .clone_published_repo_as(&graph.published, &graph.moderator_keys)
        .await?;

    // Proposal branch: one commit ahead of the published main.
    moderator_clone
        .git_ok(
            ["checkout", "-b", "pr/moderator-suggestion"],
            "git checkout -b pr/moderator-suggestion",
        )
        .await?;
    std::fs::write(
        moderator_clone.dir().join("suggestion.md"),
        "a change offered as a proposal\n",
    )?;
    moderator_clone
        .git_ok(["add", "suggestion.md"], "git add suggestion.md")
        .await?;
    moderator_clone
        .git_ok(
            ["commit", "-m", "moderator suggestion", "--no-gpg-sign"],
            "git commit (proposal branch)",
        )
        .await?;

    // Direct-to-main commit the moderator may not land.
    moderator_clone
        .git_ok(["checkout", "main"], "git checkout main")
        .await?;
    std::fs::write(
        moderator_clone.dir().join("moderator.md"),
        "a change the moderator may not land directly\n",
    )?;
    moderator_clone
        .git_ok(["add", "moderator.md"], "git add moderator.md")
        .await?;
    moderator_clone
        .git_ok(
            ["commit", "-m", "moderator commit", "--no-gpg-sign"],
            "git commit (main)",
        )
        .await?;

    let out = moderator_clone
        .nostr_push_expecting_failure(["origin", "main", "pr/moderator-suggestion"])
        .await?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .to_lowercase();
    assert!(
        combined.contains("isn't listed as a maintainer"),
        "the branch refspec should be rejected by the maintainer-listing check, got: {combined}",
    );

    // The proposal refspec pushed alongside the rejected branch still
    // landed its events on the repository relays.
    let proposal_events = harness
        .relay("default")
        .events(
            Filter::new()
                .author(moderator_pubkey)
                .kinds(vec![Kind::GitPatch, KIND_PULL_REQUEST]),
        )
        .await?;
    assert!(
        !proposal_events.is_empty(),
        "the pr/ refspec pushed alongside the rejected branch must still produce its proposal events",
    );

    // The rejected branch produced no authoritative state anywhere.
    for events in [
        harness
            .grasp("repo")
            .events(Filter::new().author(moderator_pubkey).kind(KIND_REPO_STATE))
            .await?,
        harness
            .relay("default")
            .events(Filter::new().author(moderator_pubkey).kind(KIND_REPO_STATE))
            .await?,
    ] {
        assert!(
            events.is_empty(),
            "a moderator must never produce an authoritative state event: {events:?}",
        );
    }

    Ok(())
}
