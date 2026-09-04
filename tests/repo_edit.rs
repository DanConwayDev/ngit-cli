//! Normal-path coverage for named repository relationship edits.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use ngit::{
    login::user::PrivateGitRelayList,
    repo_ref::{format_grasp_server_url_as_clone_url, format_grasp_server_url_as_relay_url},
};
use nostr::nips::nip46::{NostrConnectEventBuilder, NostrConnectMessage, NostrConnectRequest};
use nostr_connect::signer::{
    NostrConnectKeys, NostrConnectRemoteSigner, NostrConnectSignerActions,
};
use nostr_sdk::prelude::*;
use test_harness::{
    Harness, PublishRepoOpts, UnavailableTcpEndpoint, tag_value, tag_values, tag_values_multiple,
};

const SIGNER_READY_DEADLINE: Duration = Duration::from_secs(10);
const SIGNER_PROBE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct SignerRequestCounts {
    nip44_decrypt: AtomicUsize,
}

struct CountSignerRequests(Arc<SignerRequestCounts>);

impl NostrConnectSignerActions for CountSignerRequests {
    fn approve(&self, _public_key: &PublicKey, request: &NostrConnectRequest) -> bool {
        if matches!(request, NostrConnectRequest::Nip44Decrypt { .. }) {
            self.0.nip44_decrypt.fetch_add(1, Ordering::SeqCst);
        }
        true
    }
}

async fn signer_relay_client(relay_url: &RelayUrl) -> Result<Client> {
    let client = Client::default();
    client.add_relay(relay_url.clone()).await?;
    client.connect().await;
    Ok(client)
}

async fn send_nip46_message(
    client: &Client,
    from: &Keys,
    to: PublicKey,
    message: NostrConnectMessage,
) -> Result<()> {
    let event = NostrConnectEventBuilder::new(to, message).finalize(from)?;
    client.send_event(&event).await?;
    Ok(())
}

/// Prove the remote signer's ephemeral subscription is live before spawning
/// the command under test. Each probe waits on an observable pong and the
/// whole loop has a bounded deadline.
async fn wait_until_signer_ready(relay_url: &RelayUrl, signer_pubkey: PublicKey) -> Result<()> {
    let probe_keys = Keys::generate();
    let client = signer_relay_client(relay_url).await?;
    client
        .subscribe(
            Filter::new()
                .pubkey(probe_keys.public_key())
                .kind(Kind::NostrConnect)
                .limit(0),
        )
        .await?;
    let mut notifications = client.notifications();
    let deadline = tokio::time::Instant::now() + SIGNER_READY_DEADLINE;
    loop {
        send_nip46_message(
            &client,
            &probe_keys,
            signer_pubkey,
            NostrConnectMessage::request(&NostrConnectRequest::Ping),
        )
        .await?;
        let pong = tokio::time::timeout(SIGNER_PROBE_INTERVAL, async {
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    if event.kind == Kind::NostrConnect && event.pubkey == signer_pubkey {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        if matches!(pong, Ok(true)) {
            client.disconnect().await;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("remote signer did not answer a readiness probe before the deadline");
        }
    }
}

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

fn replace_grasp_hosting(event: &Event, keys: &Keys, grasp_server: &str) -> Result<Event> {
    let identifier = tag_value(event, "d").context("announcement identifier is missing")?;
    let clone_url =
        format_grasp_server_url_as_clone_url(grasp_server, &keys.public_key(), &identifier)?;
    let relay_url = format_grasp_server_url_as_relay_url(grasp_server)?;
    let mut tags: Vec<Tag> = event
        .tags
        .iter()
        .filter(|tag| {
            !matches!(
                tag.as_slice().first().map(String::as_str),
                Some("clone" | "relays")
            )
        })
        .cloned()
        .collect();
    tags.push(Tag::parse(["clone", clone_url.as_str()])?);
    tags.push(Tag::parse(["relays", relay_url.as_str()])?);
    Ok(EventBuilder::new(event.kind, event.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(event.created_at.as_secs() + 1))
        .finalize(keys)?)
}

fn clear_repo_event_caches(repo: &test_harness::Repo) -> Result<()> {
    for name in ["nostr-cache.lmdb", "test-global-cache.lmdb"] {
        let path = repo.dir().join(".git").join(name);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn targeted_grasp_add_uses_latest_announcement_from_account_write_relay() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("account")
    .with_grasp_server("repo")
    .with_grasp_server("current")
    .with_grasp_server("added")
    .build()
    .await?;
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("cold cache targeted edit".into()),
            identifier: Some("cold-cache-targeted-edit".into()),
            ..Default::default()
        })
        .await?;
    let author = published.maintainer_keys.public_key();
    let original = latest_announcement(&harness, author, &published.identifier).await?;
    let original_grasp = harness.grasp("repo").url().to_string();
    let current_grasp = harness.grasp("current").url().to_string();
    let added_grasp = harness.grasp("added").url().to_string();
    let account_relay = harness.relay("account").url().to_string();

    let relay_list = EventBuilder::new(Kind::RelayList, "")
        .tag(Tag::parse(["r", account_relay.as_str(), "write"])?)
        .custom_created_at(Timestamp::from_secs(original.created_at.as_secs() + 1))
        .finalize(&published.maintainer_keys)?;
    publish_to_relay(harness.relay("default").url(), &[&relay_list]).await?;

    // Model a prior replacement which moved hosting away from the original
    // grasp. It is available from both its newly declared repository relay and
    // the author's write relay, while the bootstrap relay still holds the
    // original announcement. A cold client that selects the original cannot
    // learn the new repository relay from that stale event, so the account
    // relay is the stable discovery route across the hosting change.
    let current = replace_grasp_hosting(&original, &published.maintainer_keys, &current_grasp)?;
    publish_to_relay(&account_relay, &[&current]).await?;
    publish_to_relay(&harness.grasp("current").relay_url(), &[&current]).await?;
    clear_repo_event_caches(&publisher)?;

    edit_ok(&publisher, &["--add-grasp-server", &added_grasp]).await?;

    let edited = harness
        .grasp("added")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .context("new grasp server did not receive the edited announcement")?;
    let clones = tag_values(&edited, "clone");
    assert!(
        clones.iter().any(|clone| clone.starts_with(&current_grasp)),
        "the targeted add must preserve the latest announcement's current grasp server: {clones:?}",
    );
    assert!(
        clones.iter().any(|clone| clone.starts_with(&added_grasp)),
        "the targeted add must include the requested grasp server: {clones:?}",
    );
    assert!(
        !clones
            .iter()
            .any(|clone| clone.starts_with(&original_grasp)),
        "the targeted add must not resurrect hosting from the stale bootstrap announcement: {clones:?}",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_refuses_when_an_account_write_relay_cannot_be_read() -> Result<()> {
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let author = published.maintainer_keys.public_key();
    let filter = Filter::new().author(author).kind(Kind::GitRepoAnnouncement);
    let before = harness
        .grasp("repo")
        .events(filter.clone())
        .await?
        .into_iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("initial announcement is missing from the grasp relay")?;

    drop(
        harness
            .take_relay("default")
            .context("default relay is missing")?,
    );
    let refused = publisher
        .ngit(["repo", "edit", "--description", "must not publish"])
        .output()
        .await
        .context("failed to spawn ngit repo edit")?;
    assert!(
        !refused.status.success(),
        "repo edit must fail closed when an account write relay cannot be read",
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("account write relay"),
        "the refusal should identify the incomplete announcement refresh: {stderr}",
    );

    let after = harness
        .grasp("repo")
        .events(filter)
        .await?
        .into_iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("announcement disappeared from the grasp relay")?;
    assert_eq!(
        after.id, before.id,
        "a failed refresh must not publish an announcement replacement",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_metadata_edit_does_not_request_private_relay_list_decryption() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let relay_url = RelayUrl::parse(harness.relay("default").url())?;

    // Publish a real account-private relay list so an unnecessary discovery
    // attempt must ask the bunker to decrypt rather than quietly finding no
    // kind-10318 event.
    let local_signer = Arc::new(ngit::signer::NgitSigner::Keys(
        published.maintainer_keys.clone(),
    ));
    let mut private_relays = PrivateGitRelayList::new(vec![relay_url.clone()])?;
    let private_relay_event = private_relays.to_event(&local_signer).await?;
    publish_to_relay(harness.relay("default").url(), &[&private_relay_event]).await?;

    let app_keys = Keys::generate();
    let remote_signer_keys = Keys::generate();
    let remote_signer = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: remote_signer_keys.clone(),
            user: published.maintainer_keys.clone(),
        },
        [relay_url.clone()],
        None,
        None,
    )?;
    let bunker_uri = remote_signer.bunker_uri().to_string();
    let app_key = app_keys.secret_key().to_secret_hex();
    let request_counts = Arc::new(SignerRequestCounts::default());
    let signer_actions = CountSignerRequests(Arc::clone(&request_counts));
    let signer_task = tokio::spawn(async move { remote_signer.serve(signer_actions).await });
    wait_until_signer_ready(&relay_url, remote_signer_keys.public_key()).await?;

    let mut edit = publisher.ngit([
        "--bunker-uri",
        &bunker_uri,
        "--bunker-app-key",
        &app_key,
        "repo",
        "edit",
        "--description",
        "edited without private discovery",
    ]);
    edit.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), edit.output()).await;
    signer_task.abort();
    let output = output
        .context("public repo edit did not finish before the remote-signer deadline")?
        .context("failed to spawn public repo edit with remote signer")?;
    if !output.status.success() {
        bail!(
            "public repo edit exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    assert_eq!(
        request_counts.nip44_decrypt.load(Ordering::SeqCst),
        0,
        "a public repository with a cached announcement must not request private relay-list decryption",
    );
    let edited = latest_announcement(
        &harness,
        published.maintainer_keys.public_key(),
        &published.identifier,
    )
    .await?;
    assert_eq!(
        tag_value(&edited, "description").as_deref(),
        Some("edited without private discovery"),
        "the metadata edit should still publish normally",
    );
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
    let alice_hex = alice.to_string();
    assert_eq!(
        tag_values(&invited, "maintainers"),
        vec![alice_hex.clone(), bob.to_string()],
    );
    let alice_roles: Vec<Vec<String>> = invited
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .filter(|tag| {
            matches!(tag.first().map(String::as_str), Some("M" | "m"))
                && tag.get(1) == Some(&alice_hex)
        })
        .collect();
    assert_eq!(
        alice_roles,
        vec![vec!["M".to_string(), alice_hex]],
        "the implicit sole maintainer should become lead from the beginning without m history",
    );
    let active_m: Vec<String> = invited
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) == Some("m") && tag.len() % 2 == 1)
        .filter_map(|tag| tag.get(1).cloned())
        .collect();
    assert_eq!(active_m, vec![bob.to_string()]);
    assert!(
        active_role_start(&invited, "m", bob).is_some(),
        "the new maintainer invitation should start at the edit time",
    );

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

/// Replace every role-bearing tag (`M`/`m`/`o`/`maintainers`) on an
/// announcement with the supplied role tags, modelling history written by
/// an older ngit version.
fn replace_role_tags(event: &Event, keys: &Keys, role_tags: &[Vec<String>]) -> Result<Event> {
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
        tags.push(Tag::parse(role.clone())?);
    }
    Ok(EventBuilder::new(event.kind, event.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(event.created_at.as_secs() + 1))
        .finalize(keys)?)
}

fn svec(parts: &[&str]) -> Vec<String> {
    parts.iter().map(ToString::to_string).collect()
}

fn role_tags_naming(event: &Event, subject: PublicKey) -> Vec<Vec<String>> {
    let subject = subject.to_string();
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .filter(|tag| {
            matches!(tag.first().map(String::as_str), Some("M" | "m" | "o"))
                && tag.get(1) == Some(&subject)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_defer_continue_repair_republishes_the_active_lead_role() -> Result<()> {
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
            display_name: Some("lead self-defer repair".into()),
            identifier: Some("lead-self-defer-repair".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let alice_hex = alice.to_string();
    let original = latest_announcement(&harness, alice, &published.identifier).await?;
    let malformed = replace_role_tags(
        &original,
        &published.maintainer_keys,
        &[svec(&["M", &alice_hex, "100", "defer"])],
    )?;
    publish_to_relay(harness.relay("default").url(), &[&malformed]).await?;
    publish_to_relay(&harness.grasp("repo").relay_url(), &[&malformed]).await?;

    edit_ok(&publisher, &["--repair-self-defer", "M=continue"]).await?;

    let repaired = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        role_tags_naming(&repaired, alice),
        vec![svec(&["M", &alice_hex, "100"])],
        "continuing the lead role must republish the repaired active `M` \
         without fabricating a co-maintainer record or a departure boundary",
    );
    assert_eq!(
        tag_values(&repaired, "maintainers"),
        vec![alice_hex],
        "the compatibility projection must list the repaired lead",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_defer_continue_repair_republishes_the_active_co_maintainer_role() -> Result<()> {
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
            display_name: Some("co-maintainer self-defer repair".into()),
            identifier: Some("co-maintainer-self-defer-repair".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let alice_hex = alice.to_string();
    let original = latest_announcement(&harness, alice, &published.identifier).await?;
    let malformed = replace_role_tags(
        &original,
        &published.maintainer_keys,
        &[svec(&["m", &alice_hex, "100", "defer"])],
    )?;
    publish_to_relay(harness.relay("default").url(), &[&malformed]).await?;
    publish_to_relay(&harness.grasp("repo").relay_url(), &[&malformed]).await?;

    edit_ok(&publisher, &["--repair-self-defer", "m=continue"]).await?;

    let repaired = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        role_tags_naming(&repaired, alice),
        vec![svec(&["m", &alice_hex, "100"])],
        "continuing the co-maintainer role must republish the repaired \
         active `m` without opening or closing any other self record",
    );
    assert_eq!(
        tag_values(&repaired, "maintainers"),
        vec![alice_hex],
        "the compatibility projection must list the repaired co-maintainer",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_defer_end_repair_merges_same_role_successor_in_signed_event() -> Result<()> {
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
            display_name: Some("same-role repair merge".into()),
            identifier: Some("same-role-repair-merge".into()),
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let alice_hex = alice.to_string();
    let original = latest_announcement(&harness, alice, &published.identifier).await?;
    let malformed = replace_role_tags(
        &original,
        &published.maintainer_keys,
        &[
            svec(&["m", &alice_hex, "100", "defer"]),
            svec(&["m", &alice_hex, "200"]),
        ],
    )?;
    publish_to_relay(harness.relay("default").url(), &[&malformed]).await?;
    publish_to_relay(&harness.grasp("repo").relay_url(), &[&malformed]).await?;

    edit_ok(&publisher, &["--repair-self-defer", "m=200"]).await?;

    let repaired = latest_announcement(&harness, alice, &published.identifier).await?;
    assert_eq!(
        role_tags_naming(&repaired, alice),
        vec![svec(&["m", &alice_hex, "100", "200", "200"])],
        "the signed replacement must merge the repaired interval and successor",
    );
    assert_eq!(
        tag_values(&repaired, "maintainers"),
        vec![alice_hex],
        "the active successor must remain in the compatibility projection",
    );
    Ok(())
}
