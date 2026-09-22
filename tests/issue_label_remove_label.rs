//! Coverage for `ngit issue label --remove-label` and the
//! NIP-09 label-deletion reads behind it.
//!
//! ## Shape
//!
//! Publish a repo, create one issue, add two labels, then remove them one
//! at a time through `issue label --remove-label`. Removing the first label
//! exercises the delete-plus-republish path (the deleted kind-1985 event
//! carried two values, so the survivor is republished); removing the last
//! exercises delete-with-nothing-remaining. A removal of a label the issue
//! does not have must fail closed. Assertions read the semantic `labels`
//! array out of `issue view --json`, never literal stdout text.

use anyhow::{Context, Result};
use test_harness::{Harness, PublishRepoOpts};

async fn run_json_labels(
    publisher: &test_harness::Repo,
    args: &[&str],
) -> Result<serde_json::Value> {
    let out = publisher
        .ngit(args)
        .output()
        .await
        .context("failed to spawn ngit")?;
    assert!(
        out.status.success(),
        "`ngit {args:?}` exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(&stdout).with_context(|| format!("stdout is not valid JSON:\n{stdout}"))
}

#[tokio::test]
async fn issue_label_remove_label_round_trips_through_view() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (publisher, _published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    let create = publisher
        .ngit(["issue", "create", "--subject", "labelled", "--body", "body"])
        .output()
        .await
        .context("failed to spawn `ngit issue create`")?;
    assert!(create.status.success(), "issue create failed");

    let list: serde_json::Value = run_json_labels(&publisher, &["issue", "list", "--json"]).await?;
    let id = list
        .as_array()
        .and_then(|issues| issues.first())
        .and_then(|issue| issue.get("id"))
        .and_then(|id| id.as_str())
        .context("expected one issue with an id in list output")?
        .to_string();

    let label = publisher
        .ngit([
            "issue",
            "label",
            &id,
            "--label",
            "bug",
            "--label",
            "help-wanted",
        ])
        .output()
        .await
        .context("failed to spawn `ngit issue label`")?;
    assert!(label.status.success(), "issue label failed");

    let viewed: serde_json::Value =
        run_json_labels(&publisher, &["issue", "view", &id, "--json"]).await?;
    let labels = viewed
        .get("labels")
        .and_then(|l| l.as_array())
        .context("expected a labels array in view output")?;
    assert!(
        labels.iter().any(|l| l == "bug"),
        "bug missing in {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "help-wanted"),
        "help-wanted missing in {labels:?}"
    );

    // Remove one of two values: the doomed kind-1985 is deleted and the
    // survivor is republished, so the view must show exactly the other.
    let edit = publisher
        .ngit(["issue", "label", &id, "--remove-label", "bug"])
        .output()
        .await
        .context("failed to spawn `ngit issue label --remove-label`")?;
    assert!(edit.status.success(), "issue label --remove-label failed");

    let viewed: serde_json::Value =
        run_json_labels(&publisher, &["issue", "view", &id, "--json"]).await?;
    let labels = viewed
        .get("labels")
        .and_then(|l| l.as_array())
        .context("expected a labels array in view output")?;
    assert!(
        !labels.iter().any(|l| l == "bug"),
        "bug still present in {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "help-wanted"),
        "help-wanted lost in {labels:?}"
    );

    // Remove the last value: nothing remains to republish.
    let edit = publisher
        .ngit(["issue", "label", &id, "--remove-label", "help-wanted"])
        .output()
        .await
        .context("failed to spawn second `ngit issue label --remove-label`")?;
    assert!(edit.status.success(), "second issue label edit failed");

    let viewed: serde_json::Value =
        run_json_labels(&publisher, &["issue", "view", &id, "--json"]).await?;
    let labels = viewed
        .get("labels")
        .and_then(|l| l.as_array())
        .context("expected a labels array in view output")?;
    assert!(labels.is_empty(), "labels remain in {labels:?}");

    // Removing a label the issue does not have fails closed.
    let edit = publisher
        .ngit(["issue", "label", &id, "--remove-label", "nope"])
        .output()
        .await
        .context("failed to spawn failing `ngit issue label`")?;
    assert!(
        !edit.status.success(),
        "removing an absent label should fail closed"
    );

    Ok(())
}
