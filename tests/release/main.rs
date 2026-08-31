//! End-to-end coverage for the NIP-82 release command family.
//!
//! These scenarios assert on relay-visible events and structured JSON fields,
//! not human-facing output. URL-backed asset events are signed and published
//! directly where the URL transport is irrelevant. URL workflows use a tiny
//! bounded in-process HTTP server so they exercise the real downloader while
//! remaining hermetic.

use std::{fs, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bitcoin_hashes::sha256;
use ngit::software_release::{
    AddressPointer, ApplicationInput, AssetInput, ReleaseAssetInput, ReleaseInput,
    SOFTWARE_APPLICATION_KIND, SOFTWARE_ASSET_KIND, SOFTWARE_RELEASE_KIND, SoftwareApplication,
    SoftwareAsset, SoftwareRelease, application_event_builder, asset_event_builder,
    release_event_builder,
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
async fn release_publish_bootstraps_the_application_asset_and_release() -> Result<()> {
    const VERSION: &str = "0.1.0";
    const ASSET_BYTES: &[u8] = b"zero-state release archive\n";

    let (harness, publisher, published) = setup(0).await?;
    let server = AssetHttpServer::spawn(vec![ServedAsset {
        path: "/zero-state.tar.gz",
        body: ASSET_BYTES,
        content_type: "application/gzip",
    }])
    .await?;
    let asset_argument = format!("linux-x86_64={}/zero-state.tar.gz", server.base_url());

    let output = run_json(
        &publisher,
        &[
            "release",
            "publish",
            VERSION,
            "--asset",
            &asset_argument,
            "--notes",
            "First release from repository metadata",
            "--json",
        ],
    )
    .await?;
    server.finish().await?;

    ensure!(output["result"]["application_operation"] == "created");
    ensure!(
        output["result"]["publication"]["ordered_events"]
            .as_array()
            .context("publication events were not an array")?
            .iter()
            .map(|event| event["entity"].as_str())
            .collect::<Vec<_>>()
            == [Some("application"), Some("asset"), Some("release")]
    );

    let application = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(&published.identifier),
        "bootstrapped software application",
    )
    .await?;
    ensure!(tag_values(&application, "f") == ["linux-x86_64"]);
    let asset = SoftwareAsset::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_ASSET_KIND)
                .author(published.maintainer_keys.public_key()),
            "bootstrapped software asset",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    let release = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(format!("{}@{VERSION}", published.identifier)),
            "bootstrapped software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        asset
            .application
            .as_ref()
            .is_some_and(|application| application.coordinate == release.application.coordinate)
    );
    ensure!(
        release.application.coordinate
            == Coordinate::new(
                SOFTWARE_APPLICATION_KIND,
                published.maintainer_keys.public_key(),
            )
            .identifier(published.identifier)
    );
    ensure!(release.assets[0].event_id == asset.raw_event.id);
    let expected_head = head_commit(&publisher)?;
    ensure!(release.commit.as_deref() == Some(expected_head.as_str()));
    ensure!(output["result"]["release"]["commit"] == expected_head);
    Ok(())
}

#[tokio::test]
async fn manifest_publishes_application_metadata_and_tracked_media_to_blossom() -> Result<()> {
    const ICON_BYTES: &[u8] = b"tracked application icon fixture\n";
    const ASSET_BYTES: &[u8] = b"application metadata release archive\n";
    const COMMUNITY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    let (harness, publisher, published) = setup(0).await?;
    let asset_server = AssetHttpServer::spawn(vec![ServedAsset {
        path: "/application-metadata.tar.gz",
        body: ASSET_BYTES,
        content_type: "application/gzip",
    }])
    .await?;
    let icon_hash = sha256_hex(ICON_BYTES);
    let blossom = BlossomHttpServer::descriptor(
        "201 Created",
        &icon_hash,
        ICON_BYTES.len() as u64,
        "image/png",
    )
    .await?;

    let media_dir = publisher.dir().join("media");
    let manifest_dir = publisher.dir().join(".ngit");
    fs::create_dir_all(&media_dir).context("failed to create application media directory")?;
    fs::create_dir_all(&manifest_dir).context("failed to create release manifest directory")?;
    fs::write(media_dir.join("icon.png"), ICON_BYTES)
        .context("failed to write application icon")?;
    let manifest = format!(
        r#"schema: 1
identifier: {APP_ID}
pubkey: {pubkey}
name: Manifest Application
summary: Metadata sourced from release.yaml
description: |
  Application metadata can stay in source control.
tags: [nostr, releases]
license: MIT
website: https://example.invalid/application
repository: nostr://example.invalid/application
icon: media/icon.png
images:
  - https://cdn.example.invalid/application/screenshot.png
communities:
  - {COMMUNITY}
supported_nips: ["34", "82"]
notes: Application metadata release
publication:
  blossom_servers:
    - "{blossom_server}"
assets:
  - source: "{asset_server}/application-metadata.tar.gz"
    filename: application-metadata.tar.gz
    mime: application/gzip
    platforms: [linux-x86_64]
"#,
        pubkey = published.maintainer_keys.public_key().to_bech32()?,
        blossom_server = blossom.base_url(),
        asset_server = asset_server.base_url(),
    );
    fs::write(manifest_dir.join("release.yaml"), manifest)
        .context("failed to write application metadata release manifest")?;
    let add = publisher
        .git(["add", "media/icon.png", ".ngit/release.yaml"])
        .output()
        .await
        .context("failed to spawn git add for application metadata")?;
    ensure!(add.status.success(), "git add failed: {add:?}");
    let commit = publisher
        .git([
            "commit",
            "-m",
            "add release application metadata",
            "--no-gpg-sign",
        ])
        .output()
        .await
        .context("failed to spawn git commit for application metadata")?;
    ensure!(commit.status.success(), "git commit failed: {commit:?}");

    let output = run_json(
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
    asset_server.finish().await?;
    let blossom_url = blossom
        .blob_url()
        .context("Blossom descriptor URL missing")?
        .to_owned();
    let blossom_requests = blossom.finish().await?;
    ensure!(blossom_upload_request(&blossom_requests)?.body == ICON_BYTES);
    ensure!(output["result"]["application_operation"] == "created");
    ensure!(output["result"]["blossom"]["uploads"][0]["entity"] == "application_media");
    ensure!(output["result"]["blossom"]["uploads"][0]["field"] == "icon");

    let application = SoftwareApplication::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_APPLICATION_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(APP_ID),
            "manifest application metadata",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(application.name == "Manifest Application");
    ensure!(application.summary.as_deref() == Some("Metadata sourced from release.yaml"));
    ensure!(application.description == "Application metadata can stay in source control.\n");
    ensure!(application.topics == ["nostr", "releases"]);
    ensure!(application.license.as_deref() == Some("MIT"));
    ensure!(application.website.as_deref() == Some("https://example.invalid/application"));
    ensure!(application.repository.as_deref() == Some("nostr://example.invalid/application"));
    ensure!(application.icon.as_deref() == Some(blossom_url.as_str()));
    ensure!(application.images == ["https://cdn.example.invalid/application/screenshot.png"]);
    ensure!(application.communities == [COMMUNITY]);

    let asset = SoftwareAsset::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_ASSET_KIND)
                .author(published.maintainer_keys.public_key()),
            "application metadata release asset",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(asset.supported_nips == ["34", "82"]);
    Ok(())
}

#[tokio::test]
async fn release_publish_never_overwrites_an_unlinked_default_application() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    let application = application_event_builder(ApplicationInput {
        identifier: published.identifier.clone(),
        name: "Existing unlinked application".to_owned(),
        platforms: vec!["linux-x86_64".to_owned()],
        ..Default::default()
    })?
    .finalize(&published.maintainer_keys)?;
    publish_to_default_relay(&harness, &application).await?;
    wait_for_relay_event(&harness, application.id).await?;

    let refused = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            "0.1.0",
            "--asset",
            "linux-x86_64=https://example.invalid/should-not-download.tar.gz",
            "--json",
        ],
    )
    .await?;
    ensure!(refused["error"]["code"] == "application_not_linked");
    let unchanged = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(&published.identifier),
        "unlinked software application",
    )
    .await?;
    ensure!(unchanged.id == application.id);
    ensure!(
        harness
            .relay("default")
            .events(Filter::new().kind(SOFTWARE_ASSET_KIND))
            .await?
            .is_empty()
    );
    ensure!(
        harness
            .relay("default")
            .events(Filter::new().kind(SOFTWARE_RELEASE_KIND))
            .await?
            .is_empty()
    );
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
    fs::write(
        publisher.dir().join("CHANGELOG.md"),
        r#"# Changelog

## [Unreleased]

- Work in progress.

## [1.2.3] - 2026-08-31

Published from the release manifest

## [1.2.2] - 2026-08-01

Previous release notes.
"#,
    )
    .context("failed to write changelog")?;
    let manifest = format!(
        r#"schema: 1
application: {APP_ID}
name: Manifest-updated application
summary: Updated without downloading its image URL
icon: https://cdn.example.invalid/application-icon.png
channel: beta
release_notes: CHANGELOG.md
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
    ensure!(published_release["result"]["application_operation"] == "edited");
    let application = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(APP_ID),
        "existing software application",
    )
    .await?;
    let parsed_application =
        SoftwareApplication::parse(&application).map_err(|error| anyhow::anyhow!(error))?;
    ensure!(parsed_application.name == "Manifest-updated application");
    ensure!(
        parsed_application.icon.as_deref()
            == Some("https://cdn.example.invalid/application-icon.png")
    );
    ensure!(
        published_release["result"]["publication"]["ordered_events"][0]["event_id"]
            == application.id.to_hex()
    );
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

    ensure!(
        linux
            .application
            .as_ref()
            .map(|pointer| &pointer.coordinate)
            == Some(&release.application.coordinate)
    );
    ensure!(
        windows
            .application
            .as_ref()
            .map(|pointer| &pointer.coordinate)
            == Some(&release.application.coordinate)
    );
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
async fn local_file_publish_confirms_every_discovered_server() -> Result<()> {
    const ASSET_BYTES: &[u8] = b"local Blossom release archive\n";

    let (harness, publisher, published) = setup(0).await?;
    create_application_with_platforms(&publisher, &["linux-x86_64", "linux-aarch64"]).await?;
    fs::write(publisher.dir().join("ngit-release.zip"), ASSET_BYTES)
        .context("failed to write local release asset")?;

    let hash = sha256_hex(ASSET_BYTES);
    let primary = BlossomHttpServer::descriptor(
        "201 Created",
        &hash,
        ASSET_BYTES.len() as u64,
        "application/zip",
    )
    .await?;
    let mirror = BlossomHttpServer::descriptor(
        "201 Created",
        &hash,
        ASSET_BYTES.len() as u64,
        "application/zip",
    )
    .await?;
    let server_list = EventBuilder::new(Kind::Custom(10_063), "")
        .tags([
            Tag::parse(["server", primary.base_url()])?,
            Tag::parse(["server", mirror.base_url()])?,
        ])
        .finalize(&published.maintainer_keys)?;
    publish_to_default_relay(&harness, &server_list).await?;
    wait_for_relay_event(&harness, server_list.id).await?;

    let output = run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--file",
            "ngit-release.zip",
            "--platform",
            "linux-x86_64",
            "--platform",
            "linux-aarch64",
            "--notes",
            "Uploaded through Blossom",
            "--json",
        ],
    )
    .await?;
    let primary_url = primary
        .blob_url()
        .context("primary descriptor URL missing")?
        .to_owned();
    let primary_root = primary.base_url_with_slash();
    let mirror_root = mirror.base_url_with_slash();
    let primary_requests = primary.finish().await?;
    let mirror_requests = mirror.finish().await?;
    let primary_request = blossom_upload_request(&primary_requests)?;
    let mirror_request = blossom_upload_request(&mirror_requests)?;

    ensure!(primary_request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
    ensure!(primary_request.body == ASSET_BYTES);
    ensure!(request_header(&primary_request.head, "authorization").is_some());
    ensure!(mirror_request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
    ensure!(mirror_request.body == ASSET_BYTES);

    let blossom = &output["result"]["blossom"];
    ensure!(blossom["server_selection"]["source"] == "kind_10063");
    ensure!(blossom["server_selection"]["event_id"] == server_list.id.to_hex());
    ensure!(blossom["server_selection"]["servers"][0] == primary_root);
    ensure!(blossom["server_selection"]["servers"][1] == mirror_root);
    ensure!(blossom["uploads"][0]["sha256"] == hash);
    ensure!(blossom["uploads"][0]["size"] == ASSET_BYTES.len().to_string());
    ensure!(blossom["uploads"][0]["primary_url"] == primary_url);
    ensure!(blossom["uploads"][0]["servers"][0]["operation"] == "upload");
    ensure!(blossom["uploads"][0]["servers"][0]["status"] == "stored");
    ensure!(blossom["uploads"][0]["servers"][1]["operation"] == "upload");
    ensure!(blossom["uploads"][0]["servers"][1]["status"] == "stored");

    let asset_event = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_ASSET_KIND)
            .author(published.maintainer_keys.public_key()),
        "Blossom-backed software asset",
    )
    .await?;
    let asset = SoftwareAsset::parse(&asset_event).map_err(|error| anyhow::anyhow!(error))?;
    ensure!(asset.url.as_deref() == Some(primary_url.as_str()));
    ensure!(asset.filename.as_deref() == Some("ngit-release.zip"));
    ensure!(asset.mime == "application/zip");
    ensure!(asset.sha256 == hash);
    ensure!(asset.size == Some(ASSET_BYTES.len() as u64));
    ensure!(asset.platforms == ["linux-aarch64", "linux-x86_64"]);

    let release = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "Blossom-backed software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(release.assets.len() == 1);
    ensure!(release.assets[0].event_id == asset.raw_event.id);
    ensure!(release.platforms == ["linux-aarch64", "linux-x86_64"]);
    Ok(())
}

#[tokio::test]
async fn local_apk_manifest_upload_extracts_android_metadata() -> Result<()> {
    const CERTIFICATE_SHA256: &str =
        "e9da81bd13f11feefe7cf220311a497c8783c0eb3254e47db76e6ab6c0745310";

    let (harness, publisher, published) = setup(0).await?;
    let apk_bytes = android_apk()?;
    let dist = publisher.dir().join("dist");
    fs::create_dir_all(&dist).context("failed to create release artifact directory")?;
    fs::write(dist.join("ngit-1.2.3.apk"), &apk_bytes)
        .context("failed to write local Android release asset")?;

    let hash = sha256_hex(&apk_bytes);
    let blossom = BlossomHttpServer::descriptor(
        "201 Created",
        &hash,
        apk_bytes.len() as u64,
        "application/vnd.android.package-archive",
    )
    .await?;
    let manifest_dir = publisher.dir().join(".ngit");
    fs::create_dir_all(&manifest_dir).context("failed to create release manifest directory")?;
    let manifest = format!(
        r#"schema: 1
application: {APP_ID}
notes: "Android metadata from a local manifest asset"
publication:
  blossom_servers:
    - "{blossom_server}"
assets:
  - file: dist/ngit-{{version}}.apk
    identifier: dev.ngit.fixture
    filename: ngit-{{version}}-android-arm64-v8a.apk
    mime: application/vnd.android.package-archive
"#,
        blossom_server = blossom.base_url(),
    );
    fs::write(manifest_dir.join("release.yaml"), manifest)
        .context("failed to write local-file release manifest")?;

    let output = run_json(
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
    let primary_url = blossom
        .blob_url()
        .context("Blossom descriptor URL missing")?
        .to_owned();
    let requests = blossom.finish().await?;
    let request = blossom_upload_request(&requests)?;

    ensure!(request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
    ensure!(request.body == apk_bytes);
    ensure!(output["result"]["application_operation"] == "created");
    ensure!(
        output["result"]["publication"]["ordered_events"]
            .as_array()
            .context("publication events were not an array")?
            .iter()
            .map(|event| event["entity"].as_str())
            .collect::<Vec<_>>()
            == [Some("application"), Some("asset"), Some("release")]
    );
    ensure!(output["result"]["blossom"]["server_selection"]["source"] == "manifest");
    ensure!(output["result"]["blossom"]["uploads"][0]["sha256"] == hash);
    ensure!(
        output["result"]["blossom"]["uploads"][0]["apk_platform_inference"]["derived_platforms"][0]
            == "android-arm64-v8a"
    );
    ensure!(
        output["result"]["blossom"]["uploads"][0]["apk_platform_inference"]["package"]
            == "dev.ngit.fixture"
    );
    ensure!(
        output["result"]["blossom"]["uploads"][0]["apk_platform_inference"]["version_name"]
            == RELEASE_VERSION
    );

    let application = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(APP_ID),
        "manifest-bootstrapped Android software application",
    )
    .await?;
    ensure!(tag_values(&application, "f") == ["android-arm64-v8a"]);

    let asset = SoftwareAsset::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_ASSET_KIND)
                .author(published.maintainer_keys.public_key()),
            "manifest-backed Android software asset",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(asset.url.as_deref() == Some(primary_url.as_str()));
    ensure!(asset.filename.as_deref() == Some("ngit-1.2.3-android-arm64-v8a.apk"));
    ensure!(asset.mime == "application/vnd.android.package-archive");
    ensure!(asset.sha256 == hash);
    ensure!(asset.size == Some(apk_bytes.len() as u64));
    ensure!(asset.platforms == ["android-arm64-v8a"]);
    ensure!(asset.version_code == Some(10203));
    ensure!(asset.min_allowed_version_code.is_none());
    ensure!(asset.min_platform_version.as_deref() == Some("24"));
    ensure!(asset.target_platform_version.as_deref() == Some("35"));
    ensure!(asset.apk_certificate_hashes == [CERTIFICATE_SHA256]);

    let release = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "manifest-backed Android software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(release.assets[0].event_id == asset.raw_event.id);
    ensure!(release.platforms == ["android-arm64-v8a"]);
    Ok(())
}

#[tokio::test]
async fn tagged_release_source_publishes_without_cli_arguments() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    let apk_bytes = android_apk()?;
    let artifact_dir = publisher.dir().join("artifacts");
    fs::create_dir_all(&artifact_dir).context("failed to create artifact directory")?;
    fs::write(artifact_dir.join("app-v1.2.3.apk"), &apk_bytes)
        .context("failed to write tagged APK fixture")?;

    let hash = sha256_hex(&apk_bytes);
    let blossom = BlossomHttpServer::descriptor(
        "201 Created",
        &hash,
        apk_bytes.len() as u64,
        "application/vnd.android.package-archive",
    )
    .await?;
    let manifest_dir = publisher.dir().join(".ngit");
    fs::create_dir_all(&manifest_dir).context("failed to create manifest directory")?;
    fs::write(
        manifest_dir.join("release.yaml"),
        format!(
            r#"schema: 1
identifier: dev.ngit.fixture
name: ngit APK fixture
notes: Tagged release source
release_source: artifacts/app-{{tag}}.apk
publication:
  blossom_servers: ["{}"]
"#,
            blossom.base_url()
        ),
    )
    .context("failed to write release manifest")?;
    publisher
        .git_ok(
            ["tag", "-a", "v1.2.3", "-m", "release v1.2.3"],
            "git tag v1.2.3",
        )
        .await?;
    let expected_commit = git2::Repository::open(publisher.dir())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();

    let output = run_json(&publisher, &["release", "publish", "--json"]).await?;
    let requests = blossom.finish().await?;
    ensure!(blossom_upload_request(&requests)?.body == apk_bytes);
    ensure!(output["result"]["release"]["version"] == RELEASE_VERSION);

    let release = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier("dev.ngit.fixture@1.2.3"),
            "tag-derived software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(release.version == RELEASE_VERSION);
    ensure!(release.commit.as_deref() == Some(expected_commit.as_str()));

    let asset = SoftwareAsset::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_ASSET_KIND)
                .author(published.maintainer_keys.public_key()),
            "tag-derived APK asset",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(asset.identifier == "dev.ngit.fixture");
    ensure!(asset.version == RELEASE_VERSION);
    ensure!(asset.version_code == Some(10203));
    Ok(())
}

#[tokio::test]
async fn local_apk_rejects_platforms_absent_from_native_libraries() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    let apk_bytes = android_apk()?;
    fs::write(publisher.dir().join("ngit.apk"), apk_bytes)
        .context("failed to write local Android release asset")?;
    fs::write(
        publisher.dir().join("release.yaml"),
        format!(
            r#"schema: 1
application: {APP_ID}
assets:
  - file: ngit.apk
    identifier: dev.ngit.fixture
    platforms: [android-x86_64]
"#,
        ),
    )
    .context("failed to write release manifest")?;

    let failure = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--manifest",
            "release.yaml",
            "--json",
        ],
    )
    .await?;
    ensure!(failure["error"]["code"] == "apk_platform_conflict");
    ensure!(failure["error"]["details"]["derived_platforms"][0] == "android-arm64-v8a");
    ensure!(failure["error"]["details"]["conflicting_platforms"][0] == "android-x86_64");

    let events = harness
        .relay("default")
        .events(Filter::new().author(published.maintainer_keys.public_key()))
        .await?;
    ensure!(
        events.iter().all(|event| ![
            SOFTWARE_APPLICATION_KIND,
            SOFTWARE_ASSET_KIND,
            SOFTWARE_RELEASE_KIND,
        ]
        .contains(&event.kind)),
        "APK preflight failure published NIP-82 events"
    );
    Ok(())
}

#[tokio::test]
async fn url_apks_cannot_claim_to_be_platform_agnostic() -> Result<()> {
    const APK_BYTES: &[u8] = b"remote APK fixture";

    let (harness, publisher, published) = setup(0).await?;
    let server = AssetHttpServer::spawn(vec![ServedAsset {
        path: "/application.apk",
        body: APK_BYTES,
        content_type: "application/vnd.android.package-archive",
    }])
    .await?;
    let url = format!("{}/application.apk", server.base_url());
    let failure = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--platform-agnostic-asset",
            &url,
            "--notes",
            "This remote APK has no inspectable platform metadata",
            "--json",
        ],
    )
    .await?;
    server.finish().await?;

    ensure!(failure["error"]["code"] == "apk_platform_conflict");
    let events = harness
        .relay("default")
        .events(
            Filter::new()
                .kinds([
                    SOFTWARE_APPLICATION_KIND,
                    SOFTWARE_ASSET_KIND,
                    SOFTWARE_RELEASE_KIND,
                ])
                .author(published.maintainer_keys.public_key()),
        )
        .await?;
    ensure!(
        events.is_empty(),
        "remote APK platform failure published NIP-82 events"
    );
    Ok(())
}

#[tokio::test]
async fn presence_failure_stops_before_upload_and_release_signing() -> Result<()> {
    const ASSET_BYTES: &[u8] = b"unplaced Blossom release archive\n";

    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;
    fs::write(publisher.dir().join("orphan.zip"), ASSET_BYTES)
        .context("failed to write local release asset")?;

    let hash = sha256_hex(ASSET_BYTES);
    let primary = BlossomHttpServer::descriptor(
        "201 Created",
        &hash,
        ASSET_BYTES.len() as u64,
        "application/zip",
    )
    .await?;
    let mirror =
        BlossomHttpServer::error("500 Internal Server Error", "presence check failed").await?;
    let failure = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--file",
            "linux-x86_64=orphan.zip",
            "--blossom-server",
            primary.base_url(),
            "--blossom-server",
            mirror.base_url(),
            "--notes",
            "This release must not be published",
            "--json",
        ],
    )
    .await?;
    let primary_root = primary.base_url_with_slash();
    let mirror_root = mirror.base_url_with_slash();
    mirror.finish().await?;

    ensure!(failure["error"]["code"] == "blossom_publication_failed");
    let details = &failure["error"]["details"];
    ensure!(details["stage"] == "upload");
    ensure!(details["server"] == mirror_root);
    ensure!(details["release_events_signed"] == false);
    ensure!(details["release_events_published"] == false);
    ensure!(details["blossom"]["uploads"][0]["servers"][0]["status"] == "not_attempted");
    ensure!(details["blossom"]["uploads"][0]["servers"][1]["status"] == "failed");
    ensure!(details["possible_orphan_blobs"].as_array().map(Vec::len) == Some(0));
    let message = failure["error"]["message"]
        .as_str()
        .context("Blossom failure message missing")?;
    ensure!(message.contains(&primary_root));
    ensure!(message.contains(&mirror_root));
    ensure!(message.contains("not_attempted"));
    ensure!(message.contains("failed"));
    ensure!(message.contains("recovery:"));

    let release_events = harness
        .relay("default")
        .events(
            Filter::new()
                .kinds([SOFTWARE_ASSET_KIND, SOFTWARE_RELEASE_KIND])
                .author(published.maintainer_keys.public_key()),
        )
        .await?;
    ensure!(
        release_events.is_empty(),
        "failed Blossom mirroring still published NIP-82 events"
    );
    Ok(())
}

#[tokio::test]
async fn url_asset_add_preserves_the_existing_release() -> Result<()> {
    const X86_BYTES: &[u8] = b"direct x86_64 archive\n";
    const ARM_BYTES: &[u8] = b"added aarch64 archive\n";
    const RELEASED_AT: &str = "1700000000";

    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;
    let server = AssetHttpServer::spawn(vec![
        ServedAsset {
            path: "/ngit-1.2.3-linux-x86_64.tar.gz",
            body: X86_BYTES,
            content_type: "application/gzip",
        },
        ServedAsset {
            path: "/ngit-1.2.3-linux-aarch64.tar.gz",
            body: ARM_BYTES,
            content_type: "application/octet-stream",
        },
    ])
    .await?;
    let x86_url = format!("{}/ngit-1.2.3-linux-x86_64.tar.gz", server.base_url());
    let x86_asset_argument = format!("linux-x86_64={x86_url}");
    run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--asset",
            &x86_asset_argument,
            "--channel",
            "stable",
            "--notes",
            "Release state which must survive asset add",
            "--released-at",
            RELEASED_AT,
            "--json",
        ],
    )
    .await?;

    let initial = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "initial URL-backed software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    let initial_asset_events = harness
        .relay("default")
        .events(
            Filter::new()
                .kind(SOFTWARE_ASSET_KIND)
                .author(published.maintainer_keys.public_key()),
        )
        .await?;
    ensure!(initial_asset_events.len() == 1);
    let x86 =
        SoftwareAsset::parse(&initial_asset_events[0]).map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        x86.application.as_ref().map(|pointer| &pointer.coordinate)
            == Some(&initial.application.coordinate)
    );
    ensure!(x86.url.as_deref() == Some(x86_url.as_str()));
    ensure!(x86.filename.as_deref() == Some("ngit-1.2.3-linux-x86_64.tar.gz"));
    ensure!(x86.mime == "application/gzip");
    ensure!(x86.sha256 == sha256_hex(X86_BYTES));
    ensure!(x86.size == Some(X86_BYTES.len() as u64));
    ensure!(x86.platforms == ["linux-x86_64"]);
    ensure!(initial.assets[0].event_id == x86.raw_event.id);

    let arm_url = format!("{}/ngit-1.2.3-linux-aarch64.tar.gz", server.base_url());
    let added = run_json(
        &publisher,
        &[
            "release",
            "asset",
            "add",
            RELEASE_IDENTIFIER,
            "--url",
            &arm_url,
            "--platform",
            "linux-aarch64",
            "--filename",
            "ngit-1.2.3-linux-aarch64.tar.gz",
            "--mime",
            "application/gzip",
            "--min-platform-version",
            "5.15",
            "--supported-nip",
            "82",
            "--variant",
            "portable",
            "--commit",
            "cafebabe",
            "--original-url",
            "https://downloads.example.invalid/ngit-1.2.3-linux-aarch64.tar.gz",
            "--edit",
            "--json",
        ],
    )
    .await?;
    ensure!(added["result"]["operation"] == "asset_added");
    ensure!(added["result"]["previous_event_id"] == initial.raw_event.id.to_hex());
    ensure!(added["result"]["publication"]["ordered_events"][0]["entity"] == "application");
    server.finish().await?;

    let replacement = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "replacement URL-backed software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(replacement.raw_event.id != initial.raw_event.id);
    ensure!(replacement.raw_event.created_at == initial.raw_event.created_at);
    ensure!(replacement.channel == initial.channel);
    ensure!(replacement.notes == initial.notes);
    ensure!(replacement.application == initial.application);
    ensure!(replacement.commit == initial.commit);
    ensure!(replacement.platforms == ["linux-aarch64", "linux-x86_64"]);

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
    let arm = asset_named(&assets, "ngit-1.2.3-linux-aarch64.tar.gz")?;
    ensure!(arm.identifier == APP_ID);
    ensure!(arm.version == RELEASE_VERSION);
    ensure!(arm.url.as_deref() == Some(arm_url.as_str()));
    ensure!(arm.mime == "application/gzip");
    ensure!(arm.sha256 == sha256_hex(ARM_BYTES));
    ensure!(arm.size == Some(ARM_BYTES.len() as u64));
    ensure!(arm.platforms == ["linux-aarch64"]);
    ensure!(arm.min_platform_version.as_deref() == Some("5.15"));
    ensure!(arm.supported_nips == ["82"]);
    ensure!(arm.variant.as_deref() == Some("portable"));
    ensure!(arm.commit.as_deref() == Some("cafebabe"));
    ensure!(
        arm.original_url.as_deref()
            == Some("https://downloads.example.invalid/ngit-1.2.3-linux-aarch64.tar.gz")
    );
    ensure!(replacement.assets[0] == initial.assets[0]);
    ensure!(replacement.assets[1].event_id == arm.raw_event.id);
    Ok(())
}

#[tokio::test]
async fn local_file_asset_add_preserves_the_existing_release() -> Result<()> {
    const ADDED_BYTES: &[u8] = b"locally added aarch64 archive\n";
    const RELEASED_AT: &str = "1700000000";

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
            "--channel",
            "stable",
            "--notes",
            "Release state which must survive local asset add",
            "--released-at",
            RELEASED_AT,
            "--json",
        ],
    )
    .await?;
    let initial = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "initial software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;

    fs::write(publisher.dir().join("added-arm.zip"), ADDED_BYTES)
        .context("failed to write local asset-add fixture")?;
    let hash = sha256_hex(ADDED_BYTES);
    let primary = BlossomHttpServer::descriptor(
        "201 Created",
        &hash,
        ADDED_BYTES.len() as u64,
        "application/zip",
    )
    .await?;
    let added = run_json(
        &publisher,
        &[
            "release",
            "asset",
            "add",
            RELEASE_IDENTIFIER,
            "--file",
            "added-arm.zip",
            "--platform",
            "linux-aarch64",
            "--filename",
            "ngit-1.2.3-linux-aarch64.zip",
            "--mime",
            "application/zip",
            "--blossom-server",
            primary.base_url(),
            "--edit",
            "--json",
        ],
    )
    .await?;
    let primary_url = primary
        .blob_url()
        .context("primary descriptor URL missing")?
        .to_owned();
    let requests = primary.finish().await?;
    let request = blossom_upload_request(&requests)?;

    ensure!(request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
    ensure!(request.body == ADDED_BYTES);
    ensure!(added["result"]["operation"] == "asset_added");
    ensure!(added["result"]["previous_event_id"] == initial.raw_event.id.to_hex());
    ensure!(added["result"]["blossom"]["uploads"][0]["primary_url"] == primary_url);

    let replacement = SoftwareRelease::parse(
        &single_event(
            &harness,
            Filter::new()
                .kind(SOFTWARE_RELEASE_KIND)
                .author(published.maintainer_keys.public_key())
                .identifier(RELEASE_IDENTIFIER),
            "replacement software release",
        )
        .await?,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(replacement.raw_event.id != initial.raw_event.id);
    ensure!(replacement.raw_event.created_at == initial.raw_event.created_at);
    ensure!(replacement.channel == initial.channel);
    ensure!(replacement.notes == initial.notes);
    ensure!(replacement.application == initial.application);
    ensure!(replacement.assets.len() == 2);
    ensure!(replacement.assets[0] == initial.assets[0]);
    ensure!(replacement.platforms == ["linux-aarch64", "linux-x86_64"]);

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
    let asset = asset_named(&assets, "ngit-1.2.3-linux-aarch64.zip")?;
    ensure!(asset.url.as_deref() == Some(primary_url.as_str()));
    ensure!(asset.mime == "application/zip");
    ensure!(asset.sha256 == hash);
    ensure!(asset.size == Some(ADDED_BYTES.len() as u64));
    ensure!(asset.platforms == ["linux-aarch64"]);
    ensure!(replacement.assets[1].event_id == asset.raw_event.id);
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
async fn release_edit_preserves_legacy_commit_omission_and_allows_an_override() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    create_application(&publisher).await?;

    let asset = asset_event(&published, "legacy.tar.gz", "66", "linux-x86_64")?;
    publish_to_default_relay(&harness, &asset).await?;
    wait_for_relay_event(&harness, asset.id).await?;
    let legacy_release = release_event_builder(ReleaseInput {
        application: AddressPointer {
            coordinate: Coordinate::new(
                SOFTWARE_APPLICATION_KIND,
                published.maintainer_keys.public_key(),
            )
            .identifier(APP_ID),
            relay_hint: None,
        },
        version: RELEASE_VERSION.to_string(),
        channel: "main".to_string(),
        notes: "Legacy release".to_string(),
        assets: vec![ReleaseAssetInput::from_asset(
            &SoftwareAsset::parse(&asset)?,
            None,
        )],
        commit: None,
        extra_tags: Vec::new(),
        released_at: Timestamp::now(),
    })?
    .finalize(&published.maintainer_keys)?;
    publish_to_default_relay(&harness, &legacy_release).await?;
    wait_for_relay_event(&harness, legacy_release.id).await?;

    let preserved = run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--edit",
            "--notes",
            "Edited legacy release",
            "--json",
        ],
    )
    .await?;
    ensure!(preserved["result"]["release"]["commit"].is_null());
    let preserved_event = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(RELEASE_IDENTIFIER),
        "legacy release replacement",
    )
    .await?;
    ensure!(SoftwareRelease::parse(&preserved_event)?.commit.is_none());

    let expected_head = head_commit(&publisher)?;
    let overridden = run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--edit",
            "--commit",
            "main",
            "--json",
        ],
    )
    .await?;
    ensure!(overridden["result"]["release"]["commit"] == expected_head);
    let overridden_event = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(RELEASE_IDENTIFIER),
        "release replacement with commit override",
    )
    .await?;
    ensure!(
        SoftwareRelease::parse(&overridden_event)?.commit.as_deref()
            == Some(expected_head.as_str())
    );
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
    let refused = run_json_expecting_failure(
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
    ensure!(refused["error"]["code"] == "application_platform_update_required");
    let added = run_json(
        &publisher,
        &[
            "release",
            "asset",
            "add",
            RELEASE_IDENTIFIER,
            "--event",
            &second.id.to_hex(),
            "--add-application-platforms",
            "--edit",
            "--json",
        ],
    )
    .await?;
    ensure!(added["result"]["operation"] == "asset_added");
    ensure!(added["result"]["application_operation"] == "edited");
    ensure!(
        added["result"]["platform_policy"]["application_platforms_added"]
            == serde_json::json!(["linux-aarch64"])
    );

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
    let updated_application = single_event(
        &harness,
        Filter::new()
            .kind(SOFTWARE_APPLICATION_KIND)
            .author(published.maintainer_keys.public_key())
            .identifier(APP_ID),
        "platform-updated software application",
    )
    .await?;
    ensure!(tag_values(&updated_application, "f") == vec!["linux-aarch64", "linux-x86_64"]);
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
    run_success(&co_maintainer, &["repo", "accept"]).await?;

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

#[tokio::test]
async fn non_main_platform_subsets_require_explicit_compatibility_acknowledgement() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;
    create_application_with_platforms(&publisher, &["linux-x86_64", "windows-x86_64"]).await?;
    let asset = asset_event(&published, "ngit-linux-x86_64.tar.gz", "33", "linux-x86_64")?;
    publish_to_default_relay(&harness, &asset).await?;
    wait_for_relay_event(&harness, asset.id).await?;

    let main_refused = run_json_expecting_failure(
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
    ensure!(main_refused["error"]["code"] == "release_platform_coverage_incomplete");

    let beta_refused = run_json_expecting_failure(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--channel",
            "beta",
            "--asset-event",
            &asset.id.to_hex(),
            "--json",
        ],
    )
    .await?;
    ensure!(beta_refused["error"]["code"] == "partial_platform_confirmation_required");

    let published_release = run_json(
        &publisher,
        &[
            "release",
            "publish",
            RELEASE_VERSION,
            "--app",
            APP_ID,
            "--channel",
            "beta",
            "--asset-event",
            &asset.id.to_hex(),
            "--allow-partial-platforms",
            "--json",
        ],
    )
    .await?;
    ensure!(published_release["result"]["platform_policy"]["partial_release"] == true);
    ensure!(
        published_release["warnings"]
            .as_array()
            .context("release warnings were not an array")?
            .iter()
            .any(|warning| warning["code"] == "partial_platform_release")
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

fn head_commit(repo: &Repo) -> Result<String> {
    Ok(git2::Repository::open(repo.dir())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string())
}

async fn create_application(repo: &Repo) -> Result<()> {
    create_application_with_platforms(repo, &["linux-x86_64"]).await
}

async fn create_application_with_platforms(repo: &Repo, platforms: &[&str]) -> Result<()> {
    let mut args = vec![
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
    ];
    for platform in platforms {
        args.extend(["--platform", *platform]);
    }
    args.push("--json");
    let created = run_json(repo, &args).await?;
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
        application: Some(AddressPointer {
            coordinate: Coordinate::new(
                SOFTWARE_APPLICATION_KIND,
                published.maintainer_keys.public_key(),
            )
            .identifier(APP_ID),
            relay_hint: None,
        }),
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

fn android_apk() -> Result<Vec<u8>> {
    let encoded = include_str!("../fixtures/android-app-1.2.3.apk.base64")
        .split_whitespace()
        .collect::<String>();
    STANDARD
        .decode(encoded)
        .context("failed to decode signed Android APK fixture")
}

struct CapturedBlossomRequest {
    head: String,
    body: Vec<u8>,
}

struct BlossomHttpServer {
    base_url: String,
    blob_url: Option<String>,
    task: Option<JoinHandle<Result<Vec<CapturedBlossomRequest>>>>,
}

impl BlossomHttpServer {
    async fn descriptor(status: &'static str, sha256: &str, size: u64, mime: &str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind Blossom fixture")?;
        let address = listener
            .local_addr()
            .context("failed to inspect Blossom fixture address")?;
        let base_url = format!("http://{address}");
        let blob_url = format!("{base_url}/{sha256}.zip");
        let body = serde_json::json!({
            "url": blob_url,
            "sha256": sha256,
            "size": size,
            "type": mime,
            "uploaded": 1,
        })
        .to_string();
        let task = tokio::spawn(serve_blossom_placement(
            listener,
            status,
            body,
            size,
            mime.to_owned(),
        ));
        Ok(Self {
            base_url,
            blob_url: Some(blob_url),
            task: Some(task),
        })
    }

    async fn error(status: &'static str, body: &str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind Blossom fixture")?;
        let address = listener
            .local_addr()
            .context("failed to inspect Blossom fixture address")?;
        let base_url = format!("http://{address}");
        let task = tokio::spawn(serve_blossom_errors(listener, status, body.to_owned()));
        Ok(Self {
            base_url,
            blob_url: None,
            task: Some(task),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn base_url_with_slash(&self) -> String {
        format!("{}/", self.base_url)
    }

    fn blob_url(&self) -> Option<&str> {
        self.blob_url.as_deref()
    }

    async fn finish(mut self) -> Result<Vec<CapturedBlossomRequest>> {
        let task = self.task.take().context("Blossom server task missing")?;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .context("Blossom server did not terminate")?
            .context("Blossom server task panicked")?
    }
}

impl Drop for BlossomHttpServer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn serve_blossom_placement(
    listener: TcpListener,
    upload_status: &'static str,
    descriptor: String,
    size: u64,
    mime: String,
) -> Result<Vec<CapturedBlossomRequest>> {
    let responses = [
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
        format!(
            "HTTP/1.1 {upload_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{descriptor}",
            descriptor.len(),
        ),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
        ),
    ];
    let mut requests = Vec::with_capacity(responses.len());
    for response in responses {
        requests.push(serve_one_blossom_request(&listener, &response).await?);
    }
    Ok(requests)
}

async fn serve_blossom_errors(
    listener: TcpListener,
    status: &'static str,
    response_body: String,
) -> Result<Vec<CapturedBlossomRequest>> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len(),
    );
    let mut requests = Vec::with_capacity(3);
    for _ in 0..3 {
        requests.push(serve_one_blossom_request(&listener, &response).await?);
    }
    Ok(requests)
}

async fn serve_one_blossom_request(
    listener: &TcpListener,
    response: &str,
) -> Result<CapturedBlossomRequest> {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .context("timed out waiting for a Blossom request")?
        .context("failed to accept a Blossom request")?;
    let request = read_blossom_request(&mut stream).await?;
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write Blossom response")?;
    stream
        .shutdown()
        .await
        .context("failed to finish Blossom response")?;
    Ok(request)
}

async fn read_blossom_request(
    stream: &mut tokio::net::TcpStream,
) -> Result<CapturedBlossomRequest> {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;

    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
        ensure!(
            bytes.len() < MAX_REQUEST_BYTES,
            "Blossom request headers exceeded the fixture limit"
        );
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading Blossom request headers")?
            .context("failed to read Blossom request headers")?;
        ensure!(read != 0, "Blossom client closed before sending headers");
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(bytes[..header_end].to_vec())
        .context("Blossom request headers were not UTF-8")?;
    let content_length = request_header(&head, "content-length")
        .map(str::parse::<usize>)
        .transpose()
        .context("Blossom request used an invalid Content-Length")?
        .unwrap_or(0);
    let request_length = header_end
        .checked_add(content_length)
        .context("Blossom request length overflowed")?;
    ensure!(
        request_length <= MAX_REQUEST_BYTES,
        "Blossom request body exceeded the fixture limit"
    );
    while bytes.len() < request_length {
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading Blossom request body")?
            .context("failed to read Blossom request body")?;
        ensure!(read != 0, "Blossom client closed before sending its body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(CapturedBlossomRequest {
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

fn blossom_upload_request(requests: &[CapturedBlossomRequest]) -> Result<&CapturedBlossomRequest> {
    requests
        .iter()
        .find(|request| request.head.starts_with("PUT /upload HTTP/1.1\r\n"))
        .context("Blossom placement did not issue an upload")
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
