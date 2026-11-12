//! Service metadata inspection preserves signed envelopes and cache-only reads.
use anyhow::{Context, Result};
use nostr::prelude::{EventBuilder, Keys, Kind, Tag, Timestamp, ToBech32, event::FinalizeEvent};
use test_harness::{Harness, PublishRepoOpts};

#[tokio::test]
async fn event_inspection_caches_exact_coordinator_metadata() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("ci")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("ci").url().to_string();
    let (repo, _) = harness
        .publish_repo(PublishRepoOpts {
            extra_repo_relays: vec![relay.clone()],
            ..Default::default()
        })
        .await?;
    let coordinator = Keys::generate();
    let now = Timestamp::now();
    let ad = EventBuilder::new(Kind::Custom(19843), "")
        .custom_created_at(now)
        .tags([
            Tag::custom("M", ["maintainer-request"]),
            Tag::custom("X", ["request-required"]),
            Tag::custom("expiration", [(now.as_secs() + 1200).to_string()]),
        ])
        .finalize(&coordinator)?;
    harness
        .publish_ci_events(&relay, std::slice::from_ref(&ad))
        .await?;
    for offline in [false, true] {
        let author = coordinator.public_key().to_bech32()?;
        let mut args = vec![
            "ci", "inspect", "--author", &author, "--kind", "19843", "--json",
        ];
        if offline {
            args.push("--offline");
        }
        let output = repo.ngit(&args).output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("events JSON")?;
        assert_eq!(json["events"][0]["event"]["id"], ad.id.to_hex());
        assert_eq!(json["events"][0]["signature_verified"], true);
        assert_eq!(json["events"][0]["live"], true);
    }
    let output = repo
        .ngit([
            "ci",
            "inspect",
            &ad.id.to_hex(),
            "--raw",
            "--offline",
            "--json",
        ])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(raw["events"][0]["event"], serde_json::to_value(&ad)?);
    let output = repo
        .ngit(["ci", "inspect", &ad.id.to_hex(), "--offline", "--json"])
        .output()
        .await?;
    let decoded: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(decoded["events"][0]["event"].get("sig").is_none());
    assert!(
        decoded["events"][0]["event"]["tags"]
            .as_array()
            .context("decoded tags")?
            .iter()
            .any(|t| t["tag"] == "M" && t["name"] == "admission_policy")
    );
    Ok(())
}

#[tokio::test]
async fn manifest_expansion_is_explicit_verified_and_does_not_download_files() -> Result<()> {
    let blossom = test_harness::BlossomServer::start().await?;
    let file_url = blossom.blob_url(&"ab".repeat(32));
    let body = serde_json::to_vec(
        &serde_json::json!({"version": 1, "files": [[file_url, "test.txt", "reports"]]}),
    )?;
    let digest = blossom.seed_blob(&body, "application/json");
    let manifest_url = blossom.blob_url(&digest);
    blossom.add_rule(test_harness::BlossomRule::any().respond_status(
        200,
        "OK",
        std::str::from_utf8(&body)?,
    ));
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("ci")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("ci").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            extra_repo_relays: vec![relay.clone()],
            ..Default::default()
        })
        .await?;
    let make = |count: &str| -> Result<_> {
        Ok(EventBuilder::new(Kind::Custom(9841), "signed log tail")
            .tags([
                Tag::custom("a", [test_harness::repo_coordinate(&published)]),
                Tag::custom("artifacts", [&manifest_url, "", count]),
            ])
            .finalize(&Keys::generate())?)
    };
    let event = make("1")?;
    harness
        .publish_ci_events(&relay, std::slice::from_ref(&event))
        .await?;
    let output = repo
        .ngit(["ci", "inspect", &event.id.to_hex(), "--json"])
        .output()
        .await?;
    assert!(output.status.success());
    assert!(blossom.requests().is_empty());
    let output = repo
        .ngit(["ci", "inspect", &event.id.to_hex(), "--artifacts", "--json"])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        json["events"][0]["artifact_manifests"][0]["manifest"]["files"][0][1],
        "test.txt"
    );
    assert!(
        blossom
            .requests()
            .iter()
            .all(|r| r.path == format!("/{digest}"))
    );
    let bad = make("2")?;
    harness
        .publish_ci_events(&relay, std::slice::from_ref(&bad))
        .await?;
    let output = repo
        .ngit(["ci", "inspect", &bad.id.to_hex(), "--artifacts", "--json"])
        .output()
        .await?;
    assert!(!output.status.success());
    blossom.finish().await?;
    Ok(())
}
