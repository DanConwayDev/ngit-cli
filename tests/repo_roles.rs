//! `ngit repo --json` role surface and `ngit repo leave` for moderators,
//! driven end-to-end against fabricated NIP-34 role-tag announcement graphs
//! (`Harness::publish_repo_with_role_graph` /
//! `Harness::publish_fabricated_announcement`).
//!
//! Complements the unit coverage in `src/lib/repo_ref.rs` /
//! `src/bin/ngit/sub_commands/repo/mod.rs` by proving the whole pipeline —
//! relay discovery of member announcements, per-author NIP-01 consolidation,
//! role/status/source classification — against real relays. The plain
//! role-tag acceptance JSON shape (two confirmed `m` co-maintainers) is
//! already pinned by `tests/repo_accept.rs`; these tests cover what that
//! suite cannot: a lead assertion, moderators in both acknowledgement
//! states, a deprecated `maintainers`-tag-only announcement, and a
//! moderator's self-declared leave overriding another member's assignment.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use serde_json::Value;
use test_harness::{Harness, Repo, tag_value, tag_values};

/// Run `ngit repo --json` (online — member announcements live on relays)
/// and parse stdout.
async fn repo_json(repo: &Repo) -> Result<Value> {
    let out = repo
        .ngit(["repo", "--json"])
        .output()
        .await
        .context("failed to spawn ngit repo --json")?;
    anyhow::ensure!(
        out.status.success(),
        "ngit repo --json exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).context("ngit repo --json stdout is not valid JSON")
}

/// The `members` entry for `npub`, or a panic naming the missing pubkey.
fn member<'a>(json: &'a Value, npub: &str) -> &'a Value {
    json["members"]
        .as_array()
        .expect("members missing from ngit repo --json")
        .iter()
        .find(|m| m["pubkey"] == npub)
        .unwrap_or_else(|| panic!("no members entry for {npub}: {json}"))
}

#[tokio::test]
async fn role_graph_surfaces_members_roles_and_sources_in_json() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (lead_repo, graph) = harness
        .publish_repo_with_role_graph("role-graph-json")
        .await?;

    let lead_npub = graph.published.maintainer_npub.clone();
    let co_npub = graph.co_maintainer_keys.public_key().to_bech32()?;
    let moderator_npub = graph.moderator_keys.public_key().to_bech32()?;
    let unack_npub = graph
        .unacknowledged_moderator_keys
        .public_key()
        .to_bech32()?;

    let json = repo_json(&lead_repo).await?;

    assert_eq!(
        json["lead_maintainer"], lead_npub,
        "the lead's active M assertion should be reported: {json}",
    );

    let moderators = json["moderators"]
        .as_array()
        .context("moderators missing from ngit repo --json")?;
    assert!(
        moderators.contains(&Value::from(moderator_npub.clone()))
            && moderators.contains(&Value::from(unack_npub.clone())),
        "both assigned moderators should be listed: {json}",
    );
    assert_eq!(
        json["confirmed_moderators"],
        serde_json::json!([moderator_npub]),
        "only the acknowledged moderator is confirmed: {json}",
    );

    let members = json["members"]
        .as_array()
        .context("members missing from ngit repo --json")?;
    assert_eq!(members.len(), 4, "lead + co + two moderators: {json}");
    for (npub, role, status) in [
        (&lead_npub, "lead", "confirmed"),
        (&co_npub, "co-maintainer", "confirmed"),
        (&moderator_npub, "moderator", "confirmed"),
        (&unack_npub, "moderator", "invited"),
    ] {
        let entry = member(&json, npub);
        assert_eq!(entry["role"], role, "role for {npub}: {json}");
        assert_eq!(entry["status"], status, "status for {npub}: {json}");
        assert_eq!(
            entry["source"], "role_tag",
            "every member here is named by an indexed role tag: {json}",
        );
    }

    Ok(())
}

/// A repository whose only membership source is the deprecated
/// `maintainers` tag (no indexed role tags at all — the pre-role-tag wire
/// shape `arrange_init_state_d_co_maintainer` fabricates). Members must be
/// classified with `source == "maintainers_tag"`, no lead is asserted, and
/// the announcement-less local user stays invited.
#[tokio::test]
async fn deprecated_maintainers_only_announcement_reports_maintainers_tag_source() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_d_co_maintainer().await?;
    let selected_npub = state.selected_maintainer_keys.public_key().to_bech32()?;
    let my_npub = state.npub.clone();

    let json = repo_json(&repo).await?;

    assert!(
        json["lead_maintainer"].is_null(),
        "the deprecated maintainers fallback asserts no lead: {json}",
    );
    assert_eq!(json["moderators"], serde_json::json!([]));

    let members = json["members"]
        .as_array()
        .context("members missing from ngit repo --json")?;
    assert_eq!(members.len(), 2, "selected maintainer + me: {json}");

    let selected = member(&json, &selected_npub);
    assert_eq!(selected["role"], "co-maintainer");
    assert_eq!(
        selected["status"], "confirmed",
        "the selected maintainer anchors the confirmed group: {json}",
    );
    assert_eq!(selected["source"], "maintainers_tag");

    let me = member(&json, &my_npub);
    assert_eq!(me["role"], "co-maintainer");
    assert_eq!(
        me["status"], "invited",
        "I have not published an acceptance announcement: {json}",
    );
    assert_eq!(me["source"], "maintainers_tag");

    Ok(())
}

/// `ngit repo leave` for a moderator: the republished acknowledgement must
/// close the `o` self-entry with an end boundary (not delete it), and —
/// because the self-declaration takes precedence over the lead's still-live
/// `o` assignment — a subsequent consolidation from another member's clone
/// must drop the leaver from the moderator set and the members listing.
/// This is also the end-to-end proof that consolidation *sees* a
/// moderator-only member's announcements at all (discovery follows `o`
/// assignments).
#[tokio::test]
async fn moderator_leave_ends_self_role_and_consolidation_drops_them() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (lead_repo, graph) = harness
        .publish_repo_with_role_graph("moderator-leave")
        .await?;
    let moderator_pubkey = graph.moderator_keys.public_key();
    let moderator_npub = moderator_pubkey.to_bech32()?;
    let unack_npub = graph
        .unacknowledged_moderator_keys
        .public_key()
        .to_bech32()?;

    let moderator_clone = harness
        .clone_published_repo_as(&graph.published, &graph.moderator_keys)
        .await?;
    let out = moderator_clone
        .ngit(["repo", "leave"])
        .output()
        .await
        .context("failed to spawn ngit repo leave")?;
    assert!(
        out.status.success(),
        "ngit repo leave exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The republished acknowledgement: NIP-01 winner among the moderator's
    // announcements on their write relay.
    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(moderator_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .iter()
        .filter(|event| {
            tag_value(event, "d").as_deref() == Some(graph.published.identifier.as_str())
        })
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
        .context("no moderator announcement found after repo leave")?;
    let self_o_entries: Vec<Vec<String>> = announcement
        .tags
        .iter()
        .map(|t| t.as_slice().to_vec())
        .filter(|s| {
            s.first().map(String::as_str) == Some("o")
                && s.get(1) == Some(&moderator_pubkey.to_string())
        })
        .collect();
    assert_eq!(
        self_o_entries.len(),
        1,
        "leaving must keep exactly one closed o self-entry; got {self_o_entries:?}",
    );
    let entry = &self_o_entries[0];
    assert!(
        entry.len() >= 4 && entry.len().is_multiple_of(2),
        "the o self-entry must be ended (even element count of at least four): {entry:?}",
    );
    assert_eq!(
        tag_values(announcement, "M"),
        vec![graph.published.maintainer_keys.public_key().to_string()],
        "the lead's M tag must survive the leave republish verbatim",
    );

    // Consolidation from the lead's clone: the leaver's self-declaration
    // wins over the lead's still-live assignment.
    let json = {
        let out = lead_repo
            .ngit(["repo", "--json"])
            .output()
            .await
            .context("failed to spawn ngit repo --json after moderator leave")?;
        anyhow::ensure!(
            out.status.success(),
            "ngit repo --json exited non-zero after moderator leave: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        serde_json::from_slice::<Value>(&out.stdout)?
    };
    assert_eq!(
        json["moderators"],
        serde_json::json!([unack_npub]),
        "the leaver must be dropped; the unacknowledged assignment stays: {json}",
    );
    assert_eq!(json["confirmed_moderators"], serde_json::json!([]));
    let members = json["members"]
        .as_array()
        .context("members missing from ngit repo --json")?;
    assert!(
        !members.iter().any(|m| m["pubkey"] == moderator_npub),
        "a member who left must not be listed: {json}",
    );

    Ok(())
}
