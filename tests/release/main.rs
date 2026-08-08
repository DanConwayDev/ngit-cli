//! End-to-end coverage for the NIP-82 release command family.
//!
//! These scenarios assert on relay-visible events and structured JSON fields,
//! not human-facing output. URL-backed asset events are signed and published
//! directly where the URL transport is irrelevant. URL workflows use a tiny
//! bounded in-process HTTP server so they exercise the real downloader while
//! remaining hermetic.

use std::{fs, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use bitcoin_hashes::sha256;
use ngit::software_release::{
    AssetInput, SOFTWARE_APPLICATION_KIND, SOFTWARE_ASSET_KIND, SOFTWARE_RELEASE_KIND,
    SoftwareAsset, SoftwareRelease, asset_event_builder,
};
use nostr_sdk::prelude::*;
use serde_json::Value;
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedRepo, Repo};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const APP_ID: &str = "ngit-release-test";
const RELEASE_VERSION: &str = "1.2.3";
const RELEASE_IDENTIFIER: &str = "ngit-release-test@1.2.3";

#[tokio::test]
async fn application_create_links_the_repo_and_refuses_implicit_replacement() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;

    let application = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(APP_ID),
        "software application",
    )
    .await?;
    let expected_repo_coordinate = format!(
        "30617:{}:{}",
        published.maintainer_keys.public_key().to_hex(),
        published.identifier
    );
    ensure!(tag_values(&application, "a").contains(&expected_repo_coordinate));

    let duplicate = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "app",
            "init",
            "--id",
            APP_ID,
            "--name",
            "replacement without edit",
            "--json",
        ],
    )
    .await?;
    ensure!(duplicate["error"]["code"] == "application_already_exists");
    let unchanged = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(APP_ID),
        "software application after refused duplicate",
    )
    .await?;
    ensure!(unchanged.id == application.id);
    Ok(())
}

#[tokio::test]
async fn manifest_publish_downloads_assets_and_preserves_metadata() -> Result<()> {
    const LINUX_BYTES: &[u8] = b"manifest linux archive\n";
    const WINDOWS_BYTES: &[u8] = b"manifest windows archive\n";

    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;
    let server = AssetHttpServer::spawn(vec![
        ServedAsset {
            path: "/manifest-linux-1.2.3.tar.gz",
            body: LINUX_BYTES,
            content_type: "application/gzip",
        },
        ServedAsset {
            path: "/manifest-windows-1.2.3.zip",
            body: WINDOWS_BYTES,
            content_type: "application/zip",
        },
    ])
    .await?;

    let manifest_dir = publisher.dir().join(".ngit");
    fs::create_dir_all(&manifest_dir).context("failed to create release manifest directory")?;
    let manifest = format!(
        r#"schema: 1
application: {APP_ID}
channel: beta
notes: "Published from the release manifest"
assets:
  - source: "{base_url}/manifest-linux-{{version}}.tar.gz"
    filename: "ngit-{{version}}-linux-x86_64.tar.gz"
    mime: application/gzip
    platforms: [linux-x86_64]
    min_platform_version: glibc-2.31
    supported_nips: ["34", "82"]
    variant: portable
    commit: deadbeef
    min_allowed_version: 1.0.0
    original_url: "https://downloads.example.invalid/ngit-{{version}}-linux.tar.gz"
  - source: "{base_url}/manifest-windows-{{version}}.zip"
    filename: "ngit-{{version}}-windows-x86_64.zip"
    mime: application/zip
    platforms: [windows-x86_64]
    target_platform_version: "11"
"#,
        base_url = server.base_url(),
    );
    fs::write(manifest_dir.join("release.yaml"), manifest)
        .context("failed to write release manifest")?;

    let published_release = run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--manifest",
            ".ngit/release.yaml",
            "--json",
        ],
    )
    .await?;
    ensure!(published_release["result"]["operation"] == "created");
    server.finish().await?;

    let release = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "manifest software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(release.channel == "beta");
    ensure!(release.notes == "Published from the release manifest");
    ensure!(release.platforms == ["linux-x86_64", "windows-x86_64"]);

    let asset_events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(SOFTWARE_ASSET_KIND)
                .author(published.maintainer_keys.public_key()),
        )
        .await?;
    ensure!(asset_events.len() == 2);
    let assets = asset_events
        .iter()
        .map(SoftwareAsset::parse)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| anyhow::anyhow!(error))?;
    let linux = asset_named(&assets, "ngit-1.2.3-linux-x86_64.tar.gz")?;
    let windows = asset_named(&assets, "ngit-1.2.3-windows-x86_64.zip")?;

    ensure!(linux.identifier == APP_ID);
    ensure!(linux.version == RELEASE_VERSION);
    ensure!(linux.mime == "application/gzip");
    ensure!(linux.sha256 == sha256_hex(LINUX_BYTES));
    ensure!(linux.size == Some(LINUX_BYTES.len() as u64));
    ensure!(linux.platforms == ["linux-x86_64"]);
    ensure!(linux.min_platform_version.as_deref() == Some("glibc-2.31"));
    ensure!(linux.supported_nips == ["34", "82"]);
    ensure!(linux.variant.as_deref() == Some("portable"));
    ensure!(linux.commit.as_deref() == Some("deadbeef"));
    ensure!(linux.min_allowed_version.as_deref() == Some("1.0.0"));
    ensure!(
        linux.original_url.as_deref()
            == Some("https://downloads.example.invalid/ngit-1.2.3-linux.tar.gz")
    );

    ensure!(windows.identifier == APP_ID);
    ensure!(windows.version == RELEASE_VERSION);
    ensure!(windows.mime == "application/zip");
    ensure!(windows.sha256 == sha256_hex(WINDOWS_BYTES));
    ensure!(windows.size == Some(WINDOWS_BYTES.len() as u64));
    ensure!(windows.platforms == ["windows-x86_64"]);
    ensure!(windows.target_platform_version.as_deref() == Some("11"));
    ensure!(
        release
            .assets
            .iter()
            .map(|pointer| pointer.event_id)
            .collect::<Vec<_>>()
            == [linux.raw_event.id, windows.raw_event.id]
    );
    Ok(())
}

#[tokio::test]
async fn release_publish_reuses_an_asset_event_and_is_readable() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;

    let asset = asset_event(&published, "ngit-linux-x86_64.tar.gz", "11", "linux-x86_64")?;
    publish_to_default_relay(&harness, &asset).await?;
    wait_for_relay_event(&harness, asset.id).await?;

    let malformed_version = "malformed";
    let malformed_identifier = format!("{APP_ID}@{malformed_version}");
    let malformed_release = EventBuilder::new(SOFTWARE_RELEASE_KIND, "")
        .tag(Tag::parse(["d", &malformed_identifier])?)
        .finalize(&published.maintainer_keys)?;
    publish_to_default_relay(&harness, &malformed_release).await?;
    wait_for_relay_event(&harness, malformed_release.id).await?;
    let malformed_guard = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            malformed_version,
            "--app",
            APP_ID,
            "--asset-event",
            &asset.id.to_hex(),
            "--json",
        ],
    )
    .await?;
    ensure!(malformed_guard["error"]["code"] == "invalid_release_metadata");

    let published_release = run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--asset-event",
            &asset.id.to_hex(),
            "--notes",
            "First test release",
            "--json",
        ],
    )
    .await?;
    ensure!(published_release["ok"] == true);
    ensure!(published_release["result"]["operation"] == "created");

    let initial_release = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(RELEASE_IDENTIFIER),
        "software release",
    )
    .await?;
    ensure!(tag_values(&initial_release, "e") == vec![asset.id.to_hex()]);
    ensure!(tag_values(&initial_release, "f") == vec!["linux-x86_64"]);

    let duplicate_release = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--asset-event",
            &asset.id.to_hex(),
            "--json",
        ],
    )
    .await?;
    ensure!(duplicate_release["error"]["code"] == "release_already_exists");
    let unchanged_release = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(RELEASE_IDENTIFIER),
        "software release after refused duplicate",
    )
    .await?;
    ensure!(unchanged_release.id == initial_release.id);

    assert_read_apis(&publisher, &asset).await?;
    Ok(())
}

#[tokio::test]
async fn adding_an_existing_asset_preserves_release_assets_and_order() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;

    let first = asset_event(&published, "ngit-linux-x86_64.tar.gz", "11", "linux-x86_64")?;
    publish_to_default_relay(&harness, &first).await?;
    wait_for_relay_event(&harness, first.id).await?;
    run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--asset-event",
            &first.id.to_hex(),
            "--notes",
            "First test release",
            "--json",
        ],
    )
    .await?;
    let initial = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(RELEASE_IDENTIFIER),
        "initial software release",
    )
    .await?;

    let second = asset_event(
        &published,
        "ngit-linux-aarch64.tar.gz",
        "22",
        "linux-aarch64",
    )?;
    publish_to_default_relay(&harness, &second).await?;
    wait_for_relay_event(&harness, second.id).await?;
    let added = run_json(
        &publisher,
        &[
            "release",
            "asset",
            "add",
            RELEASE_IDENTIFIER,
            "--event",
            &second.id.to_hex(),
            "--edit",
            "--json",
        ],
    )
    .await?;
    ensure!(added["result"]["operation"] == "asset_added");

    let replacement = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(RELEASE_IDENTIFIER),
        "replacement software release",
    )
    .await?;
    ensure!(replacement.id != initial.id);
    ensure!(tag_values(&replacement, "e") == vec![first.id.to_hex(), second.id.to_hex()]);
    ensure!(tag_values(&replacement, "f") == vec!["linux-aarch64", "linux-x86_64"]);
    Ok(())
}

#[tokio::test]
async fn co_maintainer_cannot_publish_for_an_application_owned_by_another_maintainer() -> Result<()>
{
    let (harness, publisher, published) = setup(1).await?;
    create_application(&publisher).await?;

    let co_maintainer = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    let co_maintainer_nsec = published.additional_maintainer_keys[0]
        .secret_key()
        .to_bech32()?;
    run_success(
        &co_maintainer,
        &[
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &co_maintainer_nsec,
        ],
    )
    .await?;

    let refused = run_json_expecting_failure(
        &co_maintainer,
        &["release", "publish", "2.0.0", "--app", APP_ID, "--json"],
    )
    .await?;
    ensure!(refused["error"]["code"] == "application_author_mismatch");
    ensure!(
        refused["error"]["details"]["required_author"]
            == published.maintainer_keys.public_key().to_hex()
    );
    let viewed_application = run_json(
        &co_maintainer,
        &["release", "app", "view", APP_ID, "--json"],
    )
    .await?;
    ensure!(
        viewed_application["authority"]["blocker"] == "application_author_mismatch",
        "application view did not explain why this maintainer cannot publish"
    );

    let illicit = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.additional_maintainer_keys[0].public_key())
                .identifier(format!("{APP_ID}@2.0.0")),
        )
        .await?;
    ensure!(
        illicit.is_empty(),
        "authority failure still published a release event"
    );
    Ok(())
}

async fn setup(additional_maintainer_count: usize) -> Result<(Harness, Repo, PublishedRepo)> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let default_relay = harness.relay("default").url().to_string();
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("release-api-repository".to_string()),
            additional_maintainer_count,
            extra_repo_relays: vec![default_relay],
            ..Default::default()
        })
        .await?;
    Ok((harness, publisher, published))
}

async fn create_application(repo: &Repo) -> Result<()> {
    let created = run_json(
        repo,
        &[
            "release",
            "app",
            "init",
            "--id",
            APP_ID,
            "--name",
            "ngit release test",
            "--description",
            "Exercises the NIP-82 release API",
            "--summary",
            "release fixture",
            "--website",
            "https://example.invalid/ngit-release-test",
            "--icon",
            "https://example.invalid/ngit-release-test.png",
            "--license",
            "MIT",
            "--platform",
            "linux-x86_64",
            "--json",
        ],
    )
    .await?;
    ensure!(created["result"]["operation"] == "created");
    Ok(())
}

fn asset_event(
    published: &PublishedRepo,
    filename: &str,
    hash_pair: &str,
    platform: &str,
) -> Result<Event> {
    let sha256 = hash_pair.repeat(32);
    asset_event_builder(AssetInput {
        identifier: APP_ID.to_string(),
        version: RELEASE_VERSION.to_string(),
        url: Some(format!("https://example.invalid/releases/{filename}")),
        filename: Some(filename.to_string()),
        mime: "application/gzip".to_string(),
        sha256,
        size: Some(1024),
        platforms: vec![platform.to_string()],
        ..Default::default()
    })?
    .finalize(&published.maintainer_keys)
    .context("failed to sign software asset fixture")
}

async fn publish_to_default_relay(harness: &Harness, event: &Event) -> Result<()> {
    let default_relay = harness.relay("default").url();
    let client = Client::default();
    client
        .add_relay(default_relay)
        .await
        .with_context(|| format!("failed to add release fixture relay {default_relay}"))?;
    client.connect().await;
    let output = client
        .send_event(event)
        .to([default_relay])
        .await
        .context("failed to publish software asset fixture")?;
    client.disconnect().await;
    ensure!(
        output.failed.is_empty() && !output.success.is_empty(),
        "software asset {} publication had success={:?}, failed={:?}",
        event.id,
        output.success,
        output.failed
    );
    Ok(())
}

async fn single_event(harness: &Harness, filter: Filter, label: &str) -> Result<Event> {
    let events = harness.relay("default").events(filter).await?;
    match events.as_slice() {
        [event] => Ok(event.clone()),
        _ => bail!(
            "expected one {label} on the default relay, found {}",
            events.len()
        ),
    }
}

async fn wait_for_relay_event(harness: &Harness, event_id: EventId) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let events = harness
            .relay("default")
            .events(Filter::new().id(event_id))
            .await?;
        if events.iter().any(|event| event.id == event_id) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let visible = harness.relay("default").events(Filter::new()).await?;
            bail!(
                "software asset fixture {event_id} was ACKed but is not queryable; visible kinds/ids: {:?}",
                visible
                    .iter()
                    .map(|event| (event.kind.as_u16(), event.id.to_hex()))
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn assert_read_apis(repo: &Repo, asset_event: &Event) -> Result<()> {
    let listed = run_json(repo, &["release", "list", "--json"]).await?;
    ensure!(listed["result"]["releases"].as_array().map(Vec::len) == Some(1));
    ensure!(listed["result"]["releases"][0]["version"] == RELEASE_VERSION);
    ensure!(listed["result"]["releases"][0]["asset_count"] == 1);
    ensure!(listed["result"]["releases"][0]["validation"] == serde_json::json!([]));
    ensure!(listed["result"]["releases"][0]["derived_platforms"].is_null());

    let viewed = run_json(repo, &["release", "view", RELEASE_IDENTIFIER, "--json"]).await?;
    ensure!(viewed["result"]["assets"].as_array().map(Vec::len) == Some(1));
    ensure!(viewed["result"]["unresolved_asset_ids"] == serde_json::json!([]));

    let assets = run_json(
        repo,
        &["release", "asset", "list", RELEASE_IDENTIFIER, "--json"],
    )
    .await?;
    ensure!(assets["result"]["assets"].as_array().map(Vec::len) == Some(1));

    let asset = run_json(
        repo,
        &[
            "release",
            "asset",
            "view",
            &asset_event.id.to_hex(),
            "--release",
            RELEASE_IDENTIFIER,
            "--json",
        ],
    )
    .await?;
    ensure!(asset["result"]["asset"]["event_id"] == asset_event.id.to_hex());
    ensure!(asset["result"]["asset"]["size"] == "1024");
    Ok(())
}

async fn run_json(repo: &Repo, args: &[&str]) -> Result<Value> {
    let output = repo
        .ngit(args.iter().copied())
        .output()
        .await
        .with_context(|| format!("failed to spawn ngit {args:?}"))?;
    if !output.status.success() {
        bail!(
            "ngit {args:?} exited {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    parse_json(&output.stdout, args)
}

async fn run_json_expecting_failure(repo: &Repo, args: &[&str]) -> Result<Value> {
    let output = repo
        .ngit(args.iter().copied())
        .output()
        .await
        .with_context(|| format!("failed to spawn ngit {args:?}"))?;
    if output.status.success() {
        bail!(
            "ngit {args:?} unexpectedly succeeded\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    parse_json(&output.stdout, args)
}

async fn run_success(repo: &Repo, args: &[&str]) -> Result<()> {
    let output = repo
        .ngit(args.iter().copied())
        .output()
        .await
        .with_context(|| format!("failed to spawn ngit {args:?}"))?;
    if !output.status.success() {
        bail!(
            "ngit {args:?} exited {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

fn parse_json(stdout: &[u8], args: &[&str]) -> Result<Value> {
    serde_json::from_slice(stdout).with_context(|| {
        format!(
            "ngit {args:?} did not emit one JSON document: {}",
            String::from_utf8_lossy(stdout)
        )
    })
}

fn tag_values(event: &Event, name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let fields = tag.as_slice();
            (fields.first().map(String::as_str) == Some(name))
                .then(|| fields.get(1).cloned())
                .flatten()
        })
        .collect()
}

fn asset_named<'a>(assets: &'a [SoftwareAsset], filename: &str) -> Result<&'a SoftwareAsset> {
    assets
        .iter()
        .find(|asset| asset.filename.as_deref() == Some(filename))
        .with_context(|| format!("software asset {filename:?} was not published"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha256::Hash::hash(bytes).to_string()
}

#[derive(Clone, Copy)]
struct ServedAsset {
    path: &'static str,
    body: &'static [u8],
    content_type: &'static str,
}

struct AssetHttpServer {
    base_url: String,
    task: Option<JoinHandle<Result<()>>>,
}

impl AssetHttpServer {
    async fn spawn(assets: Vec<ServedAsset>) -> Result<Self> {
        ensure!(!assets.is_empty(), "HTTP asset fixture requires a response");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind HTTP asset fixture")?;
        let address = listener
            .local_addr()
            .context("failed to inspect HTTP asset fixture address")?;
        let task = tokio::spawn(serve_assets(listener, assets));
        Ok(Self {
            base_url: format!("http://{address}"),
            task: Some(task),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn finish(mut self) -> Result<()> {
        let task = self.task.take().context("HTTP asset server task missing")?;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .context("HTTP asset server did not terminate")?
            .context("HTTP asset server task panicked")?
    }
}

impl Drop for AssetHttpServer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn serve_assets(listener: TcpListener, mut assets: Vec<ServedAsset>) -> Result<()> {
    let request_count = assets.len();
    for _ in 0..request_count {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .context("timed out waiting for an asset request")?
            .context("failed to accept an asset request")?;
        let request = read_http_request(&mut stream).await?;
        let request_line = request
            .lines()
            .next()
            .context("asset request did not contain a request line")?;
        let mut fields = request_line.split_whitespace();
        ensure!(
            fields.next() == Some("GET"),
            "unexpected request: {request_line}"
        );
        let target = fields.next().context("asset request target was missing")?;
        let path = target.split('?').next().unwrap_or(target);
        let position = assets
            .iter()
            .position(|asset| asset.path == path)
            .with_context(|| format!("unexpected asset request path {path:?}"))?;
        let asset = assets.remove(position);
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
            asset.body.len(),
            asset.content_type,
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .context("failed to write asset response headers")?;
        stream
            .write_all(asset.body)
            .await
            .context("failed to write asset response body")?;
        stream
            .shutdown()
            .await
            .context("failed to finish asset response")?;
    }
    ensure!(
        assets.is_empty(),
        "not all HTTP asset fixtures were requested"
    );
    Ok(())
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Result<String> {
    const MAX_HEADER_BYTES: usize = 16 * 1024;

    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading an asset request")?
            .context("failed to read an asset request")?;
        ensure!(read != 0, "asset client closed before sending headers");
        request.extend_from_slice(&chunk[..read]);
        ensure!(
            request.len() <= MAX_HEADER_BYTES,
            "asset request headers exceeded {MAX_HEADER_BYTES} bytes"
        );
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).context("asset request headers were not UTF-8")
}
