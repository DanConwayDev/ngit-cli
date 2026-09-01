//! End-to-end coverage for the opt-in issue edit history surface.

use anyhow::{Context, Result};
use test_harness::{Harness, PublishRepoOpts, Repo};

async fn ngit_json<const N: usize>(repo: &Repo, args: [&str; N]) -> Result<serde_json::Value> {
    let output = repo
        .ngit(args)
        .output()
        .await
        .context("failed to run ngit command")?;
    assert!(
        output.status.success(),
        "ngit exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).context("ngit stdout was not valid JSON")
}

#[tokio::test]
async fn issue_view_history_includes_original_and_every_authorised_edit() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, _published) = harness.publish_repo(PublishRepoOpts::default()).await?;

    let created = ngit_json(
        &publisher,
        [
            "issue",
            "create",
            "--subject",
            "original subject",
            "--body",
            "original body",
            "--json",
        ],
    )
    .await?;
    let issue_id = created["id"].as_str().context("issue create omitted id")?;

    ngit_json(
        &publisher,
        [
            "issue",
            "set-subject",
            issue_id,
            "--subject",
            "final subject",
            "--offline",
            "--json",
        ],
    )
    .await?;
    ngit_json(
        &publisher,
        [
            "issue",
            "set-cover-note",
            issue_id,
            "--body",
            "final body",
            "--offline",
            "--json",
        ],
    )
    .await?;

    let viewed = ngit_json(
        &publisher,
        [
            "issue",
            "view",
            issue_id,
            "--history",
            "--offline",
            "--json",
        ],
    )
    .await?;
    assert_eq!(viewed["subject"], "final subject");
    assert_eq!(viewed["cover_note"]["body"], "final body");

    let history = viewed["edit_history"]
        .as_array()
        .context("issue view omitted edit_history")?;
    assert_eq!(history.len(), 3);
    assert_eq!(history[0]["kind"], "original");
    assert_eq!(history[0]["subject"], "original subject");
    assert_eq!(history[0]["body"], "original body");

    let subjects: Vec<&str> = history
        .iter()
        .filter(|entry| entry["kind"] == "subject")
        .filter_map(|entry| entry["subject"].as_str())
        .collect();
    assert_eq!(subjects, ["final subject"]);
    let bodies: Vec<&str> = history
        .iter()
        .filter(|entry| entry["kind"] == "description")
        .filter_map(|entry| entry["body"].as_str())
        .collect();
    assert_eq!(bodies, ["final body"]);

    Ok(())
}

#[tokio::test]
async fn issue_history_distinguishes_maintainer_and_moderator_edits() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (lead, graph) = harness
        .publish_repo_with_role_graph("issue-history-roles")
        .await?;

    let created = ngit_json(
        &lead,
        [
            "issue",
            "create",
            "--subject",
            "original subject",
            "--body",
            "original body",
            "--json",
        ],
    )
    .await?;
    let issue_id = created["id"].as_str().context("issue create omitted id")?;

    ngit_json(
        &lead,
        [
            "issue",
            "set-subject",
            issue_id,
            "--subject",
            "maintainer subject",
            "--nsec",
            &graph.co_maintainer_nsec,
            "--json",
        ],
    )
    .await?;

    ngit_json(
        &lead,
        [
            "issue",
            "set-cover-note",
            issue_id,
            "--body",
            "moderator body",
            "--nsec",
            &graph.moderator_nsec,
            "--json",
        ],
    )
    .await?;

    let viewed = ngit_json(
        &lead,
        [
            "issue",
            "view",
            issue_id,
            "--history",
            "--offline",
            "--json",
        ],
    )
    .await?;
    assert_eq!(viewed["cover_note"]["author_role"], "moderator");
    assert!(viewed["cover_note"]["by_maintainer"].is_null());

    let history = viewed["edit_history"]
        .as_array()
        .context("issue view omitted edit_history")?;
    let subject_edit = history
        .iter()
        .find(|entry| entry["subject"] == "maintainer subject")
        .context("history omitted the maintainer subject edit")?;
    assert_eq!(subject_edit["author_role"], "maintainer");
    assert_eq!(subject_edit["by_maintainer"], true);

    let description_edit = history
        .iter()
        .find(|entry| entry["body"] == "moderator body")
        .context("history omitted the moderator description edit")?;
    assert_eq!(description_edit["author_role"], "moderator");
    assert!(description_edit["by_maintainer"].is_null());

    Ok(())
}
