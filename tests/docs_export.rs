use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::tempdir;
use test_harness::Harness;

#[tokio::test]
async fn docs_export_runs_before_cache_or_network_startup() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let workspace = tempdir().context("create isolated export directory")?;
    let cache = workspace.path().join("cache-must-not-be-created");
    let mut command = repo.ngit(["__docs-export"]);
    command
        .current_dir(workspace.path())
        .env("NGIT_CACHE_DIR", &cache)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "socks5://127.0.0.1:9");

    let output = tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .context("docs export exceeded its startup deadline")??;
    assert!(
        output.status.success(),
        "docs export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let export: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse exported JSON")?;
    assert_eq!(export["schema_version"], 1);
    assert_eq!(export["product"]["id"], "ngit");
    assert_eq!(export["command"]["id"], "ngit.command");
    assert!(
        !cache.exists(),
        "docs export must not initialize the ngit cache"
    );
    Ok(())
}
