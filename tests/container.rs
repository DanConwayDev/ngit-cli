//! End-to-end coverage for OCI layout publication.

use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result, bail, ensure};
use bitcoin_hashes::sha256;
use ngit::oci::{CONTAINER_REPOSITORY_KIND, ContainerRepository, ContainerTag, OCI_IMAGE_MANIFEST};
use nostr::event::FinalizeEvent;
use nostr_sdk::prelude::{Client, Coordinate, Event, EventBuilder, Filter, Kind, Tag, Url};
use serde_json::{Value, json};
use test_harness::{
    BlossomRequest, BlossomRule, BlossomServer, Harness, LocalRelayBuilderNip42, PublishRepoOpts,
    presence_requests, upload_requests,
};

#[tokio::test]
async fn publishes_a_verified_oci_layout_to_blossom_and_nostr() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("default").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("container-source-repository".to_owned()),
            extra_repo_relays: vec![relay.clone()],
            ..Default::default()
        })
        .await?;
    let (tag_digest, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::start().await?;
    let blossom_root = format!("{}/", blossom.base_url());
    let repository_coordinate = Coordinate::new(
        Kind::GitRepoAnnouncement,
        published.maintainer_keys.public_key(),
    )
    .identifier(published.identifier.clone())
    .to_string();

    let output = repo
        .ngit([
            "container",
            "publish",
            "my-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--blossom-server",
            blossom.base_url(),
            "--source",
            "https://example.com/my-app",
            "--repo-relay-only",
            "--json",
        ])
        .output()
        .await
        .context("failed to run container publish")?;
    ensure!(
        output.status.success(),
        "container publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value =
        serde_json::from_slice(&output.stdout).context("container output was not JSON")?;
    ensure!(result["command"] == "container.publish");
    ensure!(result["result"]["repository"] == "my-app");
    ensure!(result["result"]["git_repository"] == repository_coordinate);
    ensure!(result["result"]["tags"][0]["name"] == "latest");
    ensure!(result["result"]["tags"][0]["digest"] == tag_digest);
    ensure!(
        result["result"]["blobs"]
            .as_array()
            .context("blob result was not an array")?
            .len()
            == expected_blobs.len()
    );

    let requests = blossom.finish().await?;
    assert_blob_placements(&requests, &expected_blobs)?;
    for request in upload_requests(&requests) {
        ensure!(
            request
                .header("authorization")
                .is_some_and(|value| value.starts_with("Nostr "))
        );
        ensure!(request.header("content-type") == Some("application/octet-stream"));
    }

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("my-app"),
        )
        .await?;
    let [event] = events.as_slice() else {
        bail!(
            "expected one container repository event, found {}",
            events.len()
        );
    };
    event.verify()?;
    ensure!(tag_value(event, "d") == Some("my-app"));
    ensure!(tag_value(event, "a") == Some(repository_coordinate.as_str()));
    ensure!(tag_value(event, "title") == Some("my-app"));
    ensure!(tag_value(event, "source") == Some("https://example.com/my-app"));
    ensure!(
        event.tags.iter().any(|tag| {
            matches!(tag.as_slice(), [name, tag, digest] if name == "tag" && tag == "latest" && digest == &tag_digest)
        })
    );
    ensure!(event.tags.iter().any(|tag| {
        matches!(tag.as_slice(), [name, server] if name == "server" && server == &blossom_root)
    }));
    Ok(())
}

#[tokio::test]
async fn publishes_from_the_default_container_manifest() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("default").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("manifest-container-source".to_owned()),
            extra_repo_relays: vec![relay],
            ..Default::default()
        })
        .await?;
    let layout = repo.dir().join("artifacts/my-app");
    let (tag_digest, expected_blobs) = write_layout(&layout)?;
    let blossom = BlossomServer::start().await?;
    fs::create_dir_all(repo.dir().join(".ngit"))?;
    fs::write(
        repo.dir().join(".ngit/containers.yaml"),
        format!(
            r#"schema: 1
publication:
  blossom_servers:
    - {}
containers:
  my-app:
    layout: artifacts/my-app
    title: Manifest image
    description: Published from checked-in CI settings
    source: https://example.com/manifest-image
"#,
            blossom.base_url()
        ),
    )?;

    let output = repo
        .ngit(["container", "publish", "my-app", "--json"])
        .output()
        .await
        .context("failed to publish from .ngit/containers.yaml")?;
    ensure!(
        output.status.success(),
        "manifest-backed container publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(result["result"]["tags"][0]["digest"] == tag_digest);
    ensure!(
        result["result"]["manifest_path"]
            == repo
                .dir()
                .join(".ngit/containers.yaml")
                .to_string_lossy()
                .as_ref()
    );
    assert_blob_placements(&blossom.finish().await?, &expected_blobs)?;

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("my-app"),
        )
        .await?;
    let [event] = events.as_slice() else {
        bail!("expected one manifest-backed container event")
    };
    ensure!(tag_value(event, "title") == Some("Manifest image"));
    ensure!(tag_value(event, "description") == Some("Published from checked-in CI settings"));
    ensure!(tag_value(event, "source") == Some("https://example.com/manifest-image"));
    Ok(())
}

#[tokio::test]
async fn authenticates_container_preflight_reads_on_repository_relays() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay_nip42("auth-read", LocalRelayBuilderNip42::read())
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("auth-read").url().to_string();
    let (repo, _) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("authenticated-container-repository".to_owned()),
            extra_repo_relays: vec![relay.clone()],
            ..Default::default()
        })
        .await?;
    let (_, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::start().await?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "auth-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--blossom-server",
            blossom.base_url(),
            "--json",
        ])
        .output()
        .await
        .context("failed to run container publish against an authenticated relay")?;
    ensure!(
        output.status.success(),
        "container publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value =
        serde_json::from_slice(&output.stdout).context("container output was not JSON")?;
    ensure!(
        result["result"]["relays"]
            .as_array()
            .context("relay result was not an array")?
            .iter()
            .any(|result| result["url"] == relay && result["accepted"] == true),
        "authenticated relay did not accept the event: {}; stderr: {}",
        result["result"]["relays"],
        String::from_utf8_lossy(&output.stderr)
    );

    assert_blob_placements(&blossom.finish().await?, &expected_blobs)?;
    Ok(())
}

#[tokio::test]
async fn discovers_the_publishers_blossom_server_list() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("default").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("blossom-discovery-repository".to_owned()),
            extra_repo_relays: vec![relay.clone()],
            ..Default::default()
        })
        .await?;
    let (_, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::start().await?;
    let blossom_root = format!("{}/", blossom.base_url());
    let server_list = EventBuilder::new(Kind::Custom(10_063), "")
        .tags([Tag::parse(["server", blossom.base_url()])?])
        .finalize(&published.maintainer_keys)?;
    publish_fixture_event(&relay, &server_list).await?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "discovered-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--json",
        ])
        .output()
        .await
        .context("failed to run container publish with Blossom discovery")?;
    ensure!(
        output.status.success(),
        "container publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_blob_placements(&blossom.finish().await?, &expected_blobs)?;

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("discovered-app"),
        )
        .await?;
    let [event] = events.as_slice() else {
        bail!(
            "expected one discovered container repository event, found {}",
            events.len()
        );
    };
    ensure!(event.tags.iter().any(|tag| {
        matches!(tag.as_slice(), [name, server] if name == "server" && server == &blossom_root)
    }));
    Ok(())
}

#[tokio::test]
async fn merges_state_when_one_repository_relay_is_unavailable() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("history")
    .with_grasp_server("repo")
    .build()
    .await?;
    let publication_relay = harness.relay("default").url().to_string();
    let history_relay = harness.relay("history").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("container-history-repository".to_owned()),
            extra_repo_relays: vec![publication_relay.clone(), history_relay.clone()],
            ..Default::default()
        })
        .await?;
    let (_, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::start().await?;
    let server_list = EventBuilder::new(Kind::Custom(10_063), "")
        .tags([Tag::parse(["server", blossom.base_url()])?])
        .finalize(&published.maintainer_keys)?;
    publish_fixture_event(&publication_relay, &server_list).await?;
    let repository_coordinate = Coordinate::new(
        Kind::GitRepoAnnouncement,
        published.maintainer_keys.public_key(),
    )
    .identifier(published.identifier.clone());
    let previous = ContainerRepository {
        name: "repository-relay-app".to_owned(),
        repository: repository_coordinate,
        tags: vec![ContainerTag {
            name: "stable".to_owned(),
            digest: "f".repeat(64),
        }],
        servers: vec![Url::parse("https://old-blossom.example/")?],
        title: Some("Existing title".to_owned()),
        description: None,
        source: None,
        extra_tags: vec![],
    }
    .event_builder()?
    .finalize(&published.maintainer_keys)?;
    publish_fixture_event(&history_relay, &previous).await?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "repository-relay-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--relay",
            "ws://127.0.0.1:1",
            "--json",
        ])
        .output()
        .await
        .context("failed to publish with one unavailable repository relay")?;
    ensure!(
        output.status.success(),
        "container publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_blob_placements(&blossom.finish().await?, &expected_blobs)?;

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("repository-relay-app"),
        )
        .await?;
    let latest = events
        .iter()
        .max_by_key(|event| (event.created_at, std::cmp::Reverse(event.id)))
        .context("updated container event was not published")?;
    ensure!(latest.tags.iter().any(|tag| {
        matches!(tag.as_slice(), [name, tag, digest] if name == "tag" && tag == "stable" && digest == &"f".repeat(64))
    }));
    ensure!(latest.tags.iter().any(|tag| {
        matches!(tag.as_slice(), [name, tag, ..] if name == "tag" && tag == "latest")
    }));
    Ok(())
}

#[tokio::test]
async fn publishes_when_each_blob_has_one_confirmed_blossom_copy() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("default").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("resilient-container-source".to_owned()),
            extra_repo_relays: vec![relay],
            ..Default::default()
        })
        .await?;
    let (_, expected_blobs) = write_layout(repo.dir())?;
    let unavailable = BlossomServer::start().await?;
    unavailable.add_rule(BlossomRule::upload().respond_status(
        400,
        "Bad Request",
        "fixture refuses uploads",
    ));
    let available = BlossomServer::start().await?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "resilient-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--blossom-server",
            unavailable.base_url(),
            "--blossom-server",
            available.base_url(),
            "--json",
        ])
        .output()
        .await
        .context("failed to run resilient container publish")?;
    ensure!(
        output.status.success(),
        "resilient container publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(result["format_version"] == 2);
    ensure!(result["command_status"] == "ok");
    ensure!(result.get("ok").is_none());
    ensure!(result["warnings"].as_array().is_some_and(|warnings| {
        warnings
            .iter()
            .any(|warning| warning["code"] == "blossom_replication_incomplete")
    }));
    let blobs = result["result"]["blobs"]
        .as_array()
        .context("container blobs were not an array")?;
    ensure!(blobs.len() == expected_blobs.len());
    for blob in blobs {
        let servers = blob["servers"]
            .as_array()
            .context("container blob server outcomes were not an array")?;
        ensure!(servers.iter().any(|server| server["status"] == "failed"));
        ensure!(servers.iter().any(|server| server["status"] == "stored"));
    }

    let unavailable_requests = unavailable.finish().await?;
    ensure!(
        !upload_requests(&unavailable_requests).is_empty(),
        "the unavailable server received no attempted upload"
    );
    assert_blob_placements(&available.finish().await?, &expected_blobs)?;
    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("resilient-app"),
        )
        .await?;
    ensure!(events.len() == 1, "container event was not published");
    Ok(())
}

#[tokio::test]
async fn does_not_publish_when_a_blob_has_no_confirmed_blossom_copy() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("default").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("unavailable-container-source".to_owned()),
            extra_repo_relays: vec![relay],
            ..Default::default()
        })
        .await?;
    write_layout(repo.dir())?;
    let unavailable = BlossomServer::start().await?;
    unavailable.add_rule(BlossomRule::upload().respond_status(
        400,
        "Bad Request",
        "fixture refuses uploads",
    ));

    let output = repo
        .ngit([
            "container",
            "publish",
            "unavailable-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--blossom-server",
            unavailable.base_url(),
            "--json",
        ])
        .output()
        .await
        .context("failed to run unavailable container publish")?;
    ensure!(
        !output.status.success(),
        "container publish unexpectedly succeeded"
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(result["format_version"] == 2);
    ensure!(result["command_status"] == "error");
    ensure!(result.get("ok").is_none());
    ensure!(result["error"]["code"] == "blossom_upload_failed");
    ensure!(
        result["error"]["details"]["blobs"]
            .as_array()
            .is_some_and(|blobs| blobs.len() == 1),
        "the failed blob outcome was not retained"
    );
    ensure!(!upload_requests(&unavailable.finish().await?).is_empty());

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("unavailable-app"),
        )
        .await?;
    ensure!(
        events.is_empty(),
        "a container event was published after Blossom failure"
    );
    Ok(())
}

fn write_layout(root: &std::path::Path) -> Result<(String, BTreeMap<String, Vec<u8>>)> {
    let blob_directory = root.join("blobs/sha256");
    fs::create_dir_all(&blob_directory).context("failed to create OCI blob directory")?;
    fs::write(
        root.join("oci-layout"),
        serde_json::to_vec(&json!({"imageLayoutVersion": "1.0.0"}))?,
    )?;

    let mut blobs = BTreeMap::new();
    let config = serde_json::to_vec(&json!({"architecture": "amd64", "os": "linux"}))?;
    let config_digest = write_blob(&blob_directory, config, &mut blobs)?;
    let layer = b"tiny deterministic layer\n".to_vec();
    let layer_digest = write_blob(&blob_directory, layer, &mut blobs)?;
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": OCI_IMAGE_MANIFEST,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": blobs[&config_digest].len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": format!("sha256:{layer_digest}"),
            "size": blobs[&layer_digest].len(),
        }],
    }))?;
    let manifest_digest = write_blob(&blob_directory, manifest, &mut blobs)?;
    fs::write(
        root.join("index.json"),
        serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": OCI_IMAGE_MANIFEST,
                "digest": format!("sha256:{manifest_digest}"),
                "size": blobs[&manifest_digest].len(),
                "annotations": {"org.opencontainers.image.ref.name": "latest"},
            }],
        }))?,
    )?;
    Ok((manifest_digest, blobs))
}

fn write_blob(
    directory: &std::path::Path,
    bytes: Vec<u8>,
    blobs: &mut BTreeMap<String, Vec<u8>>,
) -> Result<String> {
    let digest = sha256::Hash::hash(&bytes).to_string();
    fs::write(directory.join(&digest), &bytes)?;
    blobs.insert(digest.clone(), bytes);
    Ok(digest)
}

/// Every layout blob is placed on the single Blossom server exactly once:
/// a `404` presence check, a `PUT /upload` carrying the blob, and the
/// post-upload `200` verification.
///
/// The shared fixture serves requests until it is finished rather than
/// budgeting them, so these counts are asserted explicitly.
fn assert_blob_placements(
    requests: &[BlossomRequest],
    expected_blobs: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    ensure!(
        requests.len() == expected_blobs.len() * 3,
        "expected three Blossom requests per blob, found {}",
        requests.len()
    );
    ensure!(presence_requests(requests).len() == expected_blobs.len() * 2);
    let uploads = upload_requests(requests);
    ensure!(uploads.len() == expected_blobs.len());
    for request in uploads {
        let digest = sha256::Hash::hash(&request.body).to_string();
        ensure!(request.hash() == Some(digest.as_str()));
        ensure!(expected_blobs.get(&digest) == Some(&request.body));
    }
    Ok(())
}

fn tag_value<'a>(event: &'a nostr_sdk::prelude::Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [tag_name, value, ..] if tag_name == name => Some(value.as_str()),
        _ => None,
    })
}

async fn publish_fixture_event(relay: &str, event: &Event) -> Result<()> {
    let client = Client::default();
    client.add_relay(relay).await?;
    client.connect().await;
    let output = client.send_event(event).to([relay]).await?;
    client.disconnect().await;
    ensure!(
        output.failed.is_empty() && !output.success.is_empty(),
        "fixture event {} publication had success={:?}, failed={:?}",
        event.id,
        output.success,
        output.failed
    );
    Ok(())
}
