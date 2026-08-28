//! Normal-path coverage for named repository relationship edits.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishRepoOpts, tag_value, tag_values, tag_values_multiple};

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

async fn publish_to_default(harness: &Harness, events: &[&Event]) -> Result<()> {
    let relay_url = harness.relay("default").url();
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
    publish_to_default(&harness, &[&announcement, &state]).await?;

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
    publish_to_default(&harness, &[&announcement]).await?;

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
    let bob_npub = bob.to_bech32()?;
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
