//! End-to-end coverage for the NIP-82 release command family.
//!
//! These scenarios assert on relay-visible events and structured JSON fields,
//! not human-facing output. URL-backed asset events are signed and published
//! directly so the tests remain hermetic and do not require an HTTP server.

use anyhow::{Context, Result, bail, ensure};
use ngit::software_release::{
    AssetInput, SOFTWARE_APPLICATION_KIND, SOFTWARE_RELEASE_KIND, asset_event_builder,
};
use nostr_sdk::prelude::*;
use serde_json::Value;
use test_harness::{CloneLogin, Harness, PublishRepoOpts, PublishedRepo, Repo};

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
