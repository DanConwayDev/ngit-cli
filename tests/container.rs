//! End-to-end coverage for OCI layout publication.

use std::{collections::BTreeMap, fs, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use bitcoin_hashes::sha256;
use ngit::oci::{CONTAINER_REPOSITORY_KIND, OCI_IMAGE_MANIFEST};
use nostr::event::FinalizeEvent;
use nostr_sdk::prelude::{Client, Event, EventBuilder, Filter, Keys, Kind, Tag, ToBech32};
use serde_json::{Value, json};
use test_harness::{Harness, LocalRelayBuilderNip42};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    task::JoinHandle,
};

#[tokio::test]
async fn publishes_a_verified_oci_layout_to_blossom_and_nostr() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let (tag_digest, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::spawn(expected_blobs.len()).await?;
    let blossom_root = format!("{}/", blossom.base_url());
    let relay = harness.relay("default").url().to_string();
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "my-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--blossom-server",
            blossom.base_url(),
            "--relay",
            &relay,
            "--source",
            "https://example.com/my-app",
            "--nsec",
            &nsec,
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
    ensure!(requests.len() == expected_blobs.len());
    for request in requests {
        ensure!(request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
        ensure!(
            request_header(&request.head, "authorization")
                .is_some_and(|value| value.starts_with("Nostr "))
        );
        ensure!(request_header(&request.head, "content-type") == Some("application/octet-stream"));
        let digest = sha256::Hash::hash(&request.body).to_string();
        ensure!(expected_blobs.get(&digest) == Some(&request.body));
    }

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(keys.public_key())
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
async fn authenticates_container_preflight_reads_on_publication_relays() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay_nip42("auth-read", LocalRelayBuilderNip42::read())
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let (_, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::spawn(expected_blobs.len()).await?;
    let relay = harness.relay("auth-read").url().to_string();
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "auth-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--blossom-server",
            blossom.base_url(),
            "--relay",
            &relay,
            "--nsec",
            &nsec,
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
    ensure!(result["result"]["relays"][0]["url"] == relay);
    ensure!(result["result"]["relays"][0]["accepted"] == true);

    let requests = blossom.finish().await?;
    ensure!(requests.len() == expected_blobs.len());
    Ok(())
}

#[tokio::test]
async fn discovers_the_publishers_blossom_server_list() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let (_, expected_blobs) = write_layout(repo.dir())?;
    let blossom = BlossomServer::spawn(expected_blobs.len()).await?;
    let blossom_root = format!("{}/", blossom.base_url());
    let relay = harness.relay("default").url().to_string();
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let server_list = EventBuilder::new(Kind::Custom(10_063), "")
        .tags([Tag::parse(["server", blossom.base_url()])?])
        .finalize(&keys)?;
    publish_fixture_event(&relay, &server_list).await?;

    let output = repo
        .ngit([
            "container",
            "publish",
            "discovered-app",
            "--layout",
            repo.dir().to_str().context("test path was not UTF-8")?,
            "--relay",
            &relay,
            "--nsec",
            &nsec,
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
    let requests = blossom.finish().await?;
    ensure!(requests.len() == expected_blobs.len());

    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(CONTAINER_REPOSITORY_KIND)
                .author(keys.public_key())
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

struct CapturedRequest {
    head: String,
    body: Vec<u8>,
}

struct BlossomServer {
    base_url: String,
    task: Option<JoinHandle<Result<Vec<CapturedRequest>>>>,
}

impl BlossomServer {
    async fn spawn(expected_requests: usize) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind Blossom fixture")?;
        let address = listener.local_addr()?;
        let base_url = format!("http://{address}");
        let response_root = base_url.clone();
        let task = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(expected_requests);
            for _ in 0..expected_requests {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(10), listener.accept())
                        .await
                        .context("timed out waiting for a Blossom upload")??;
                let request = read_request(&mut stream).await?;
                let digest = request_header(&request.head, "x-sha-256")
                    .context("upload omitted X-SHA-256")?;
                let response_body = json!({
                    "url": format!("{response_root}/{digest}"),
                    "sha256": digest,
                    "size": request.body.len(),
                    "type": "application/octet-stream",
                    "uploaded": 1,
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
                requests.push(request);
            }
            Ok(requests)
        });
        Ok(Self {
            base_url,
            task: Some(task),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn finish(mut self) -> Result<Vec<CapturedRequest>> {
        let task = self.task.take().context("Blossom fixture task missing")?;
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .context("Blossom fixture did not finish")??
    }
}

impl Drop for BlossomServer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Result<CapturedRequest> {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
        ensure!(
            bytes.len() < MAX_REQUEST_BYTES,
            "request headers were too large"
        );
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading Blossom request headers")??;
        ensure!(read != 0, "client closed before request headers completed");
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(bytes[..header_end].to_vec())?;
    let content_length = request_header(&head, "content-length")
        .context("request omitted Content-Length")?
        .parse::<usize>()?;
    let request_length = header_end
        .checked_add(content_length)
        .context("request length overflowed")?;
    ensure!(
        request_length <= MAX_REQUEST_BYTES,
        "request body was too large"
    );
    while bytes.len() < request_length {
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading Blossom request body")??;
        ensure!(read != 0, "client closed before request body completed");
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(CapturedRequest {
        head,
        body: bytes[header_end..request_length].to_vec(),
    })
}

fn request_header<'a>(head: &'a str, wanted: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(wanted).then(|| value.trim())
    })
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
