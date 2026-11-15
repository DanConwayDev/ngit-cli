use anyhow::{Context, Result};
use test_harness::{Harness, PublishRepoOpts};

async fn json(repo: &test_harness::Repo, args: &[&str]) -> Result<serde_json::Value> {
    let out = repo.ngit(args).output().await?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[tokio::test]
async fn removing_labels_converges_for_cached_and_fresh_readers() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    json(
        &publisher,
        &[
            "issue",
            "create",
            "--subject",
            "labels",
            "--body",
            "body",
            "--json",
        ],
    )
    .await?;
    let listed = json(&publisher, &["issue", "list", "--json"]).await?;
    let id = listed[0]["id"].as_str().context("missing issue id")?;
    json(
        &publisher,
        &[
            "issue",
            "label",
            id,
            "--label",
            "bug",
            "--label",
            "help-wanted",
            "--json",
        ],
    )
    .await?;
    let coordinate = format!(
        "nostr://{}/{}/{}",
        published.maintainer_npub,
        urlencoding::encode(&harness.grasp("repo").relay_url()),
        published.identifier
    );
    let warm = harness.fresh_repo()?;
    let observed = json(
        &warm,
        &["--repo", &coordinate, "issue", "view", id, "--json"],
    )
    .await?;
    assert_eq!(
        observed["labels"],
        serde_json::json!(["bug", "help-wanted"])
    );
    let result = json(
        &publisher,
        &["issue", "label", id, "--remove-label", "BUG", "--json"],
    )
    .await?;
    assert_eq!(result["labels"], serde_json::json!(["help-wanted"]));
    // Existing readers must converge without clearing their cache.
    let observed = json(
        &warm,
        &["--repo", &coordinate, "issue", "view", id, "--json"],
    )
    .await?;
    assert_eq!(observed["labels"], serde_json::json!(["help-wanted"]));
    let reader = harness.fresh_repo()?;
    let observed = json(
        &reader,
        &["--repo", &coordinate, "issue", "view", id, "--json"],
    )
    .await?;
    assert_eq!(observed["labels"], serde_json::json!(["help-wanted"]));
    json(
        &publisher,
        &[
            "issue",
            "label",
            id,
            "--remove-label",
            "help-wanted",
            "--json",
        ],
    )
    .await?;
    let observed = json(
        &warm,
        &["--repo", &coordinate, "issue", "view", id, "--json"],
    )
    .await?;
    assert_eq!(observed["labels"], serde_json::json!([]));
    // Adding only bug differs from the original two-label assignment.
    json(
        &publisher,
        &["issue", "label", id, "--label", "bug", "--json"],
    )
    .await?;
    let observed = json(
        &warm,
        &["--repo", &coordinate, "issue", "view", id, "--json"],
    )
    .await?;
    assert_eq!(observed["labels"], serde_json::json!(["bug"]));
    let missing = publisher
        .ngit(["issue", "label", id, "--remove-label", "missing"])
        .output()
        .await?;
    assert!(!missing.status.success());
    Ok(())
}
