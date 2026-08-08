//! End-to-end coverage for the NIP-82 release command family.

use anyhow::{Context, Result, bail, ensure};
use ngit::software_release::SOFTWARE_APPLICATION_KIND;
use nostr_sdk::prelude::*;
use serde_json::Value;
use test_harness::{Harness, PublishRepoOpts, PublishedRepo, Repo};

const APP_ID: &str = "ngit-release-test";

#[tokio::test]
async fn application_create_links_the_repo_and_refuses_implicit_replacement() -> Result<()> {
    let (harness, publisher, published) = setup(0).await?;

    let created = run_json(
        &publisher,
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
