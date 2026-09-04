//! Malformed (unparseable) role records: malformed is not departure.
//!
//! An author whose own role records are exclusively garbage is *blocked* —
//! excluded from confirmation without being read as a signed departure —
//! and the repository stays readable through the selected-coordinate
//! fallback with author-scoped health. A validly departed author, by
//! contrast, must keep hitting the "no longer a confirmed maintainer; run
//! `ngit repo follow-lead`" redirect even when an incidental invalid
//! self-`defer` sits beside the numeric departure. The predicate-level
//! rules are pinned by unit tests in `src/lib/repo_ref.rs`; these tests
//! prove the whole pipeline — relay discovery, per-author consolidation,
//! readability carve-out, health JSON — against real relays.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishRepoOpts, tag_value};

async fn latest_announcement(
    harness: &Harness,
    author: PublicKey,
    identifier: &str,
) -> Result<Event> {
    harness
        .relay("default")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .context("no announcement for author on the default relay")
}

/// Replace every role-bearing tag (`M`/`m`/`o`/`maintainers`) on an
/// announcement with the supplied raw role tags, modelling history written
/// by an older or third-party client.
fn replace_role_tags(event: &Event, keys: &Keys, role_tags: &[Vec<&str>]) -> Result<Event> {
    let mut tags: Vec<Tag> = event
        .tags
        .iter()
        .filter(|tag| {
            !matches!(
                tag.as_slice().first().map(String::as_str),
                Some("M" | "m" | "o" | "maintainers")
            )
        })
        .cloned()
        .collect();
    for role in role_tags {
        tags.push(Tag::parse(role.iter().copied())?);
    }
    Ok(EventBuilder::new(event.kind, event.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(event.created_at.as_secs() + 1))
        .finalize(keys)?)
}

async fn publish_to_relay(relay_url: &str, events: &[&Event]) -> Result<()> {
    let client = Client::default();
    client.add_relay(relay_url).await?;
    client.connect().await;
    for event in events {
        let output = client.send_event(event).to([relay_url]).await?;
        anyhow::ensure!(output.failed.is_empty(), "relay rejected event: {output:?}");
    }
    client.disconnect().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclusively_malformed_lead_stays_readable_with_author_scoped_health() -> Result<()> {
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
            display_name: Some("malformed lead readable".into()),
            identifier: Some("malformed-lead-readable".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let alice_hex = alice.to_string();
    let alice_npub = alice.to_bech32()?;
    let original = latest_announcement(&harness, alice, &published.identifier).await?;
    let malformed = replace_role_tags(
        &original,
        &published.maintainer_keys,
        &[vec!["M", &alice_hex, "abc"]],
    )?;
    publish_to_relay(harness.relay("default").url(), &[&malformed]).await?;
    publish_to_relay(&harness.grasp("repo").relay_url(), &[&malformed]).await?;

    let out = publisher.ngit(["repo", "--json"]).output().await?;
    anyhow::ensure!(
        out.status.success(),
        "the repository must stay readable via the selected-coordinate fallback\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        json["name"], "malformed lead readable",
        "the fallback must retain the selected announcement's metadata: {json}",
    );
    assert_eq!(
        json["confirmed_maintainers"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        0,
        "a malformed-only author must not be confirmed: {json}",
    );
    let problems = json["health"]["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let malformed_problem = problems
        .iter()
        .find(|problem| problem["code"] == "malformed_role_record")
        .with_context(|| format!("no malformed_role_record health problem: {json}"))?;
    assert_eq!(malformed_problem["scope"], "author");
    assert_eq!(malformed_problem["author"], alice_npub.as_str());
    assert_eq!(malformed_problem["blocks_author"], true);
    assert_eq!(
        json["health"]["status"], "error",
        "the affected signer sees their blocking record as an error: {json}",
    );

    let refused = publisher
        .ngit(["repo", "edit", "--description", "must not publish"])
        .output()
        .await?;
    anyhow::ensure!(
        !refused.status.success(),
        "the affected signer's announcement mutations must be gated",
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("unparseable"),
        "the gate should explain the malformed records: {stderr}",
    );
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        malformed.id,
        "a gated edit must not publish a replacement announcement",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validly_departed_author_redirects_despite_stray_invalid_defer() -> Result<()> {
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
            display_name: Some("departed stray defer".into()),
            identifier: Some("departed-stray-defer".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let alice_hex = alice.to_string();
    let original = latest_announcement(&harness, alice, &published.identifier).await?;
    // A signed numeric departure beside an incidental invalid self-`defer`
    // in another role: the departure is authoritative.
    let departed = replace_role_tags(
        &original,
        &published.maintainer_keys,
        &[
            vec!["m", &alice_hex, "100", "200"],
            vec!["o", &alice_hex, "50", "defer"],
        ],
    )?;
    publish_to_relay(harness.relay("default").url(), &[&departed]).await?;
    publish_to_relay(&harness.grasp("repo").relay_url(), &[&departed]).await?;

    std::fs::write(publisher.dir().join("stale.md"), "must not publish\n")?;
    publisher.git_ok(["add", "stale.md"], "git add").await?;
    publisher
        .git_ok(
            ["commit", "-m", "stale departed push", "--no-gpg-sign"],
            "git commit",
        )
        .await?;
    let rejected = publisher
        .nostr_push_expecting_failure(["origin", "HEAD:main"])
        .await?;
    let rejection = format!(
        "{}\n{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr),
    );
    assert!(
        rejection.contains("no longer a confirmed maintainer")
            && rejection.contains("ngit repo follow-lead"),
        "the departed coordinate must redirect via follow-lead: {rejection}",
    );
    Ok(())
}
