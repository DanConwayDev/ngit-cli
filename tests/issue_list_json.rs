//! Regression coverage for stdout pollution in issue JSON commands.
//!
//! The online fetch run by `fetching_with_report`
//! (`src/lib/client.rs`) used to print its `no updates` / `updates: X`
//! summary to **stdout** via `println!`, landing immediately before the
//! JSON array the command emits. That made `ngit issue list --json | jq .`
//! fail to parse. The summary now goes to stderr, so stdout carries only
//! the JSON document.
//!
//! ## Shape
//!
//! Publish a repo, create one issue through the CLI (which caches it
//! locally), then run `ngit issue list --json`. Because the issue is
//! already in the local cache, the fetch reports `no updates` — the exact
//! condition that used to corrupt stdout. The assertion is that the whole
//! of stdout parses as the expected JSON array.

use anyhow::{Context, Result};
use test_harness::{Harness, PublishRepoOpts};

#[tokio::test]
async fn issue_json_stdout_is_valid_when_relay_updates_are_reported() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (publisher, _published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    // Land a real issue on the relay and in the publisher's local cache.
    // `ngit issue create` publishes via `send_events`, whose per-relay
    // `send_event_to` saves the event to the local cache. The next online
    // fetch therefore has nothing new to report.
    let create = publisher
        .ngit([
            "issue",
            "create",
            "--subject",
            "a test issue",
            "--body",
            "body",
            "--json",
        ])
        .output()
        .await
        .context("failed to spawn `ngit issue create`")?;
    assert!(
        create.status.success(),
        "`ngit issue create` exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        create.status,
        String::from_utf8_lossy(&create.stdout),
        String::from_utf8_lossy(&create.stderr),
    );
    let create_stderr = String::from_utf8_lossy(&create.stderr);
    assert!(
        create_stderr.contains("updates:") || create_stderr.contains("no updates"),
        "test did not exercise relay update reporting:\n{create_stderr}"
    );

    let create_stdout = String::from_utf8_lossy(&create.stdout).to_string();
    let create_json: serde_json::Value = serde_json::from_str(&create_stdout)
        .with_context(|| format!("issue create stdout is not valid JSON:\n{create_stdout}"))?;
    assert_eq!(create_json["command_status"], "ok");
    assert_eq!(create_json["action"], "created");
    assert_eq!(create_json["entity"], "issue");
    assert_eq!(create_json["subject"], "a test issue");
    assert!(
        create_json["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("nevent1")),
        "issue create did not return a nevent id: {create_stdout}"
    );

    let out = publisher
        .ngit(["issue", "list", "--json"])
        .output()
        .await
        .context("failed to spawn `ngit issue list --json`")?;
    assert!(
        out.status.success(),
        "`ngit issue list --json` exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The whole of stdout must parse as a JSON array. A stray `no updates`
    // line on stdout (the pre-fix behaviour) makes this fail — exactly what
    // breaks `ngit issue list --json | jq .`.
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("stdout is not valid JSON:\n{stdout}"))?;
    let issues = json
        .as_array()
        .with_context(|| format!("expected a JSON array, got: {stdout}"))?;
    assert_eq!(issues.len(), 1, "unexpected issue count in: {stdout}");
    assert_eq!(issues[0]["subject"], "a test issue");

    Ok(())
}
