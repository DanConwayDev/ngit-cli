//! Normal-path coverage for named repository relationship edits.

use std::{collections::BTreeMap, time::Duration};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{
    Harness, PublishRepoOpts, UnavailableTcpEndpoint, tag_value, tag_values, tag_values_multiple,
};

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
        .context("repository announcement is missing")
}

async fn latest_state(harness: &Harness, identifier: &str) -> Result<Event> {
    harness
        .relay("default")
        .events(Filter::new().kind(Kind::Custom(30618)))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .context("repository state is missing")
}

fn state_refs(event: &Event) -> BTreeMap<String, String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let tag = tag.as_slice();
            let name = tag.first()?;
            (name == "HEAD" || name.starts_with("refs/"))
                .then(|| (name.clone(), tag.get(1).cloned().unwrap_or_default()))
        })
        .collect()
}

fn replace_clone(event: &Event, keys: &Keys, clone_url: &str) -> Result<Event> {
    let mut tags: Vec<Tag> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) != Some("clone"))
        .cloned()
        .collect();
    tags.push(Tag::parse(["clone", clone_url])?);
    Ok(EventBuilder::new(event.kind, event.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(event.created_at.as_secs() + 1))
        .finalize(keys)?)
}

async fn edit_ok(repo: &test_harness::Repo, args: &[&str]) -> Result<()> {
    let mut command = vec!["repo", "edit"];
    command.extend_from_slice(args);
    let output = repo.ngit(command).output().await?;
    if !output.status.success() {
        bail!(
            "ngit repo edit exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn targeted_setting_actions_preserve_derived_and_untouched_values() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("extra")
    .with_grasp_server("repo")
    .with_grasp_server("backup")
    .with_vanilla_git_server("mirror")
    .build()
    .await?;
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("targeted repository settings".into()),
            identifier: Some("targeted-repository-settings".into()),
            ..Default::default()
        })
        .await?;
    let author = published.maintainer_keys.public_key();
    let primary_grasp = harness.grasp("repo").url().to_string();
    let backup_grasp = harness.grasp("backup").url().to_string();
    let extra_relay = harness.relay("extra").url().to_string();
    let mirror = harness.vanilla_git_server("mirror").url().to_string();

    edit_ok(
        &publisher,
        &[
            "--add-grasp-server",
            &backup_grasp,
            "--add-additional-relay",
            &extra_relay,
            "--add-additional-clone",
            &mirror,
            "--add-hashtag",
            "#Rust",
        ],
    )
    .await?;
    let added = latest_announcement(&harness, author, &published.identifier).await?;
    let clones = tag_values(&added, "clone");
    assert_eq!(clones.len(), 3);
    assert!(clones.contains(&mirror));
    assert!(
        clones.iter().any(|clone| clone.starts_with(&primary_grasp)),
        "the existing grasp-derived clone must be preserved",
    );
    assert!(
        clones.iter().any(|clone| clone.starts_with(&backup_grasp)),
        "the added grasp server must contribute its derived clone",
    );
    let relays = tag_values(&added, "relays");
    assert_eq!(relays.len(), 3);
    assert!(relays.contains(&extra_relay));
    assert_eq!(tag_values_multiple(&added, "t"), vec!["rust"]);

    edit_ok(
        &publisher,
        &[
            "--remove-grasp-server",
            &backup_grasp,
            "--remove-additional-relay",
            &extra_relay,
            "--remove-additional-clone",
            &mirror,
            "--remove-hashtag",
            "rust",
        ],
    )
    .await?;
    let removed = latest_announcement(&harness, author, &published.identifier).await?;
    let clones = tag_values(&removed, "clone");
    assert_eq!(clones.len(), 1);
    assert!(clones[0].starts_with(&primary_grasp));
    assert_eq!(tag_values(&removed, "relays").len(), 1);
    assert!(tag_values_multiple(&removed, "t").is_empty());

    Ok(())
}

#[tokio::test]
async fn grasp_derived_entries_cannot_be_removed_as_additional_settings() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let author = published.maintainer_keys.public_key();
    let before = latest_announcement(&harness, author, &published.identifier).await?;
    let grasp_relay = tag_values(&before, "relays")
        .into_iter()
        .next()
        .context("grasp-derived relay is missing")?;
    let grasp_clone = tag_values(&before, "clone")
        .into_iter()
        .next()
        .context("grasp-derived clone is missing")?;

    for (flag, value) in [
        ("--remove-additional-relay", grasp_relay),
        ("--remove-additional-clone", grasp_clone),
    ] {
        let refused = publisher
            .ngit(["repo", "edit", flag, &value])
            .output()
            .await?;
        assert!(!refused.status.success(), "{flag} must reject {value}");
    }
    assert_eq!(
        latest_announcement(&harness, author, &published.identifier)
            .await?
            .id,
        before.id,
        "a refused derived-setting edit must not publish an announcement",
    );

    Ok(())
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

fn active_role_start(event: &Event, letter: &str, subject: PublicKey) -> Option<u64> {
    let subject = subject.to_string();
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .find(|tag| {
            tag.first().map(String::as_str) == Some(letter)
                && tag.get(1) == Some(&subject)
                && tag.len() % 2 == 1
        })
        .and_then(|tag| tag.last()?.parse().ok())
}

#[tokio::test]
async fn named_add_and_remove_change_only_that_relationship() -> Result<()> {
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
            display_name: Some("named roster edits".into()),
            identifier: Some("named-roster-edits".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob = Keys::generate().public_key();
    let bob_npub = bob.to_bech32()?;

    edit_ok(&publisher, &["--add-maintainer", &bob_npub]).await?;
    let invited = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        tag_values(&invited, "maintainers"),
        vec![alice.to_string(), bob.to_string()],
    );
    assert_eq!(tag_values_multiple(&invited, "M"), vec![alice.to_string()]);
    let active_m: Vec<String> = invited
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) == Some("m") && tag.len() % 2 == 1)
        .filter_map(|tag| tag.get(1).cloned())
        .collect();
    assert_eq!(active_m, vec![bob.to_string()]);

    let refused = publisher
        .ngit([
            "repo",
            "edit",
            "--remove-maintainer",
            &bob_npub,
            "--no-lead-maintainer",
        ])
        .output()
        .await?;
    assert!(
        !refused.status.success(),
        "--no-lead-maintainer must not erase an existing lead",
    );
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        invited.id,
        "a refused governance change must not publish an announcement",
    );

    edit_ok(&publisher, &["--remove-maintainer", &bob_npub]).await?;
    let removed = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(tag_values(&removed, "maintainers"), vec![alice.to_string()]);
    assert_eq!(tag_values_multiple(&removed, "M"), vec![alice.to_string()]);
    let bob_history: Vec<Vec<String>> = removed
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .filter(|tag| {
            tag.first().map(String::as_str) == Some("m") && tag.get(1) == Some(&bob.to_string())
        })
        .collect();
    assert_eq!(bob_history.len(), 1);
    assert_eq!(bob_history[0].len(), 4, "Bob's invitation should be ended");
    assert!(
        bob_history[0][2].parse::<u64>().is_ok() && bob_history[0][3].parse::<u64>().is_ok(),
        "Bob's invitation history should retain numeric start/end boundaries",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removal_hands_current_state_to_the_remaining_maintainer() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("remove-state-author".into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    harness.publish_user_relay_list(&bob_keys).await?;
    let bob_repo = harness
        .clone_published_repo_as(&published, &bob_keys)
        .await?;
    let accepted = bob_repo.ngit(["repo", "accept"]).output().await?;
    assert!(
        accepted.status.success(),
        "Bob must be confirmed before removal"
    );

    bob_repo
        .git_ok(
            [
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                "state authored by Bob",
            ],
            "git commit as Bob",
        )
        .await?;
    bob_repo.nostr_push(["origin", "main"]).await?;
    let bob_state = latest_state(&harness, &published.identifier).await?;
    assert_eq!(bob_state.pubkey, bob, "Bob's pushed state must be current");

    edit_ok(&alice_repo, &["--remove-maintainer", &bob.to_bech32()?]).await?;

    let removed = latest_announcement(&harness, alice, &published.identifier).await?;
    assert!(
        !tag_values(&removed, "maintainers").contains(&bob.to_string()),
        "the successful edit must remove Bob from Alice's active roster",
    );
    let handed_off = latest_state(&harness, &published.identifier).await?;
    assert_eq!(
        handed_off.pubkey, alice,
        "the removing maintainer must author the resolved replacement state",
    );
    assert_ne!(handed_off.id, bob_state.id);
    assert_eq!(
        state_refs(&handed_off),
        state_refs(&bob_state),
        "the handoff must preserve the complete resolved ref map",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_state_handoff_does_not_remove_the_maintainer() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("failed-remove-state-author".into()),
            additional_maintainer_count: 1,
            extra_repo_relays: vec![harness.relay("default").url().to_string()],
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    harness.publish_user_relay_list(&bob_keys).await?;
    let bob_repo = harness
        .clone_published_repo_as(&published, &bob_keys)
        .await?;
    let accepted = bob_repo.ngit(["repo", "accept"]).output().await?;
    assert!(
        accepted.status.success(),
        "Bob must be confirmed before removal"
    );
    bob_repo
        .git_ok(
            [
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                "state that must survive removal",
            ],
            "git commit as Bob",
        )
        .await?;
    bob_repo.nostr_push(["origin", "main"]).await?;
    let bob_state = latest_state(&harness, &published.identifier).await?;
    assert_eq!(bob_state.pubkey, bob, "Bob's pushed state must be current");

    // Publish otherwise equivalent announcements that point every confirmed
    // maintainer at a test-owned endpoint which fails promptly. The removal
    // can still resolve its membership preview from the relay, but cannot
    // reproduce the state on a git server and therefore must not close Bob's
    // assignment.
    let unavailable = UnavailableTcpEndpoint::start().await?;
    let dead_url = format!("http://{}/repo.git", unavailable.addr());
    let alice_before = latest_announcement(&harness, alice, &published.identifier).await?;
    let bob_before = latest_announcement(&harness, bob, &published.identifier).await?;
    let alice_dead = replace_clone(&alice_before, &published.maintainer_keys, &dead_url)?;
    let bob_dead = replace_clone(&bob_before, &bob_keys, &dead_url)?;
    publish_to_relay(harness.relay("default").url(), &[&alice_dead, &bob_dead]).await?;
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        alice_dead.id,
    );

    let failed = tokio::time::timeout(
        Duration::from_secs(10),
        alice_repo
            .ngit(["repo", "edit", "--remove-maintainer", &bob.to_bech32()?])
            .output(),
    )
    .await
    .context("maintainer removal did not fail promptly")??;
    assert!(
        !failed.status.success(),
        "removal must fail when the current state cannot be handed off",
    );
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        alice_dead.id,
        "a failed handoff must leave the membership announcement untouched",
    );
    assert_eq!(
        latest_state(&harness, &published.identifier).await?.id,
        bob_state.id,
        "a failed handoff must not publish a partial replacement state",
    );
    Ok(())
}

#[tokio::test]
async fn reciprocal_add_refuses_divergent_state_without_publishing() -> Result<()> {
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
            display_name: Some("reciprocal state collision".into()),
            identifier: Some("reciprocal-state-collision".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = Keys::generate();
    let bob = bob_keys.public_key();
    let started = Timestamp::now().as_secs().to_string();
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier(published.identifier.clone()),
            Tag::parse(["M", &alice.to_string(), &started])?,
            Tag::parse(["m", &bob.to_string(), &started])?,
            Tag::parse(["maintainers", &alice.to_string(), &bob.to_string()])?,
        ])
        .finalize(&bob_keys)?;
    let state = EventBuilder::new(Kind::Custom(30618), "")
        .tags([
            Tag::identifier(published.identifier.clone()),
            Tag::parse([
                "refs/heads/experiment",
                "2222222222222222222222222222222222222222",
            ])?,
        ])
        .finalize(&bob_keys)?;
    publish_to_relay(harness.relay("default").url(), &[&announcement, &state]).await?;

    let before = latest_announcement(&harness, alice, &published.identifier).await?;
    let output = publisher
        .ngit([
            "--json",
            "repo",
            "edit",
            "--add-maintainer",
            &bob.to_bech32()?,
        ])
        .output()
        .await?;
    assert!(
        !output.status.success(),
        "an auto-confirming divergent state must block the invitation",
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(error["category"], "membership_state_conflict");
    assert!(
        error["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        before.id,
        "a refused reciprocal add must not publish an announcement",
    );
    Ok(())
}

#[tokio::test]
async fn reciprocal_add_refuses_joining_another_maintainer_component() -> Result<()> {
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
            identifier: Some("reciprocal-component-collision".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = Keys::generate();
    let bob = bob_keys.public_key();
    let tom = Keys::generate().public_key();
    let started = Timestamp::now().as_secs().to_string();
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier(published.identifier.clone()),
            Tag::parse(["M", &alice.to_string(), &started])?,
            Tag::parse(["m", &bob.to_string(), &started])?,
            Tag::parse(["m", &tom.to_string(), &started])?,
            Tag::parse([
                "maintainers",
                &alice.to_string(),
                &bob.to_string(),
                &tom.to_string(),
            ])?,
        ])
        .finalize(&bob_keys)?;
    publish_to_relay(harness.relay("default").url(), &[&announcement]).await?;

    let before = latest_announcement(&harness, alice, &published.identifier).await?;
    let output = publisher
        .ngit(["repo", "edit", "--add-maintainer", &bob.to_bech32()?])
        .output()
        .await?;
    assert!(!output.status.success(), "component joins must fail closed");
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        before.id,
        "a refused component join must not publish an announcement",
    );
    Ok(())
}

#[tokio::test]
async fn indirect_auto_confirmation_refuses_before_publication() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("indirect-auto-confirmation".into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let carol_keys = published.additional_maintainer_keys[0].clone();
    let carol = carol_keys.public_key();
    harness.publish_user_relay_list(&carol_keys).await?;
    let carol_repo = harness
        .clone_published_repo_as(&published, &carol_keys)
        .await?;
    let accepted = carol_repo.ngit(["repo", "accept"]).output().await?;
    assert!(accepted.status.success(), "Carol must be confirmed first");

    let bob_keys = Keys::generate();
    let bob = bob_keys.public_key();
    let started = Timestamp::now().as_secs().to_string();
    let bob_announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier(published.identifier.clone()),
            Tag::parse(["m", &bob.to_string(), &started])?,
            Tag::parse(["m", &carol.to_string(), &started])?,
            Tag::parse(["maintainers", &bob.to_string(), &carol.to_string()])?,
        ])
        .finalize(&bob_keys)?;
    publish_to_relay(harness.relay("default").url(), &[&bob_announcement]).await?;

    let before = latest_announcement(&harness, alice, &published.identifier).await?;
    let output = alice_repo
        .ngit([
            "--json",
            "repo",
            "edit",
            "--add-maintainer",
            &bob.to_bech32()?,
        ])
        .output()
        .await?;
    assert!(
        !output.status.success(),
        "an indirect auto-confirmation must fail closed",
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(error["category"], "membership_indirect_confirmation");
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        before.id,
        "a refused indirect confirmation must publish nothing",
    );
    Ok(())
}

#[tokio::test]
async fn acknowledgement_adopts_the_confirmed_acceptance_start() -> Result<()> {
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
            display_name: Some("acknowledged acceptance".into()),
            identifier: Some("acknowledged-acceptance".into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    harness.publish_user_relay_list(&bob_keys).await?;
    let bob_repo = harness
        .clone_published_repo_as(&published, &bob_keys)
        .await?;
    let accepted = bob_repo.ngit(["repo", "accept"]).output().await?;
    if !accepted.status.success() {
        bail!(
            "ngit repo accept exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            accepted.status,
            String::from_utf8_lossy(&accepted.stdout),
            String::from_utf8_lossy(&accepted.stderr),
        );
    }
    let bob_announcement = latest_announcement(&harness, bob, &published.identifier).await?;
    let accepted_at = active_role_start(&bob_announcement, "m", bob)
        .context("Bob's acceptance has no numeric active self-role start")?;
    let bob_npub = bob.to_bech32()?;
    edit_ok(&publisher, &["--acknowledge-maintainer-change", &bob_npub]).await?;

    let alice_announcement = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        active_role_start(&alice_announcement, "m", bob),
        Some(accepted_at),
        "Alice should retain Bob's signed acceptance boundary",
    );
    Ok(())
}

#[tokio::test]
async fn lead_candidate_prepares_the_full_roster_before_handover() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("prepared handover".into()),
            identifier: Some("prepared-handover".into()),
            additional_maintainer_count: 2,
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    let carol = published.additional_maintainer_keys[1].public_key();
    let moderator = Keys::generate().public_key();
    let bob_npub = bob.to_bech32()?;
    let initial = latest_announcement(&harness, alice, &published.identifier).await?;
    let mut roster_tags: Vec<Tag> = initial.tags.iter().cloned().collect();
    roster_tags.push(Tag::parse([
        "o",
        &moderator.to_string(),
        &Timestamp::now().as_secs().to_string(),
    ])?);
    let roster_with_moderator = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(roster_tags)
        .custom_created_at(Timestamp::from_secs(initial.created_at.as_secs() + 1))
        .finalize(&published.maintainer_keys)?;
    publish_to_relay(harness.relay("default").url(), &[&roster_with_moderator]).await?;
    publish_to_relay(
        &harness.grasp("repo").relay_url(),
        &[&roster_with_moderator],
    )
    .await?;
    let visible_roster = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        visible_roster.id, roster_with_moderator.id,
        "the moderator-bearing roster must be the current lead event",
    );
    harness.publish_user_relay_list(&bob_keys).await?;
    let bob_repo = harness
        .clone_published_repo_as(&published, &bob_keys)
        .await?;
    let accepted = bob_repo.ngit(["repo", "accept"]).output().await?;
    if !accepted.status.success() {
        bail!(
            "ngit repo accept exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            accepted.status,
            String::from_utf8_lossy(&accepted.stdout),
            String::from_utf8_lossy(&accepted.stderr),
        );
    }
    let before_prepare = bob_repo
        .ngit(["repo", "--json"])
        .output()
        .await
        .context("failed to inspect moderator roster before preparation")?;
    assert!(before_prepare.status.success());
    let before_prepare: serde_json::Value = serde_json::from_slice(&before_prepare.stdout)?;
    assert!(
        before_prepare["moderators"]
            .as_array()
            .is_some_and(|moderators| !moderators.is_empty()),
        "the moderator assignment must resolve before preparation: {before_prepare}",
    );
    let bob_origin = bob_repo
        .config("remote.origin.url")
        .await?
        .context("Bob's origin is missing before repository edit")?;
    assert!(bob_repo.config("nostr.repo").await?.is_none());

    let before = latest_announcement(&harness, alice, &published.identifier).await?;
    let premature = alice_repo
        .ngit(["repo", "edit", "--lead-maintainer", &bob_npub])
        .output()
        .await?;
    assert!(
        !premature.status.success(),
        "handover must wait for the candidate's complete active roster",
    );
    assert_eq!(
        latest_announcement(&harness, alice, &published.identifier)
            .await?
            .id,
        before.id,
        "a premature handover must not publish",
    );

    edit_ok(&bob_repo, &["--lead-maintainer", &bob_npub]).await?;
    assert_eq!(
        bob_repo.config("remote.origin.url").await?,
        Some(bob_origin),
        "repository edits must not change the selected remote",
    );
    assert_eq!(
        bob_repo.config("nostr.repo").await?,
        None,
        "repository edits must not re-root nostr.repo on their publisher",
    );
    let prepared = latest_announcement(&harness, bob, &published.identifier).await?;
    assert_eq!(
        tag_values(&prepared, "maintainers")
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        [alice.to_string(), bob.to_string(), carol.to_string()]
            .into_iter()
            .collect(),
        "the candidate should actively list the complete current roster",
    );
    assert!(active_role_start(&prepared, "M", bob).is_some());
    let prepared_moderator = prepared
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .find(|tag| {
            tag.first().map(String::as_str) == Some("o")
                && tag.get(1) == Some(&moderator.to_string())
        })
        .with_context(|| {
            format!(
                "prepared lead omitted the moderator assignment: {:?}",
                prepared
                    .tags
                    .iter()
                    .map(|tag| tag.as_slice())
                    .collect::<Vec<_>>()
            )
        })?;
    assert_eq!(
        prepared_moderator.len() % 2,
        1,
        "prepared lead must actively retain the moderator assignment",
    );

    edit_ok(&alice_repo, &["--lead-maintainer", &bob_npub]).await?;
    let handed_over = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        tag_values(&handed_over, "maintainers")
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        [alice.to_string(), bob.to_string()].into_iter().collect(),
    );
    assert!(handed_over.tags.iter().any(|tag| {
        let tag = tag.as_slice();
        tag.first().map(String::as_str) == Some("m")
            && tag.get(1) == Some(&carol.to_string())
            && tag.last().map(String::as_str) == Some("defer")
    }));
    Ok(())
}
