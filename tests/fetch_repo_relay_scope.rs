//! Repository collaboration reads must use the relays declared by the
//! repository, while publication may still fan out to a user's write relays.
//!
//! This regression keeps those policies separate. An issue is created through
//! ngit and therefore reaches both the repository relay and the maintainer's
//! personal relay. A closed-status event is then placed only on the personal
//! relay. Online issue listing must ignore it both in an established working
//! copy and while a bare, hint-free URL bootstraps the announcement from the
//! default relay. The status becomes visible only when the identical event is
//! also available from the repository relay.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{Harness, PublishRepoOpts};

#[tokio::test]
async fn collaboration_reads_use_repo_relays_without_disabling_personal_fanout() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("repo relay reader".into()),
            identifier: Some("repo-relay-read-scope".into()),
            ..Default::default()
        })
        .await?;

    let create = publisher
        .ngit([
            "issue",
            "create",
            "--subject",
            "relay scoped issue",
            "--body",
            "body",
        ])
        .output()
        .await
        .context("failed to spawn `ngit issue create`")?;
    assert_success("ngit issue create", &create)?;

    let issue_filter = Filter::new()
        .author(published.maintainer_keys.public_key())
        .kind(Kind::GitIssue);
    let personal_issues = harness
        .relay("default")
        .events(issue_filter.clone())
        .await?;
    let issue = personal_issues
        .iter()
        .find(|event| tag_value(event, "subject") == Some("relay scoped issue"))
        .context("created issue did not reach the maintainer's personal relay")?;
    let issue_id = issue.id;

    let repo_issues = harness
        .grasp("repo")
        .events(issue_filter)
        .await
        .context("query repository relay for created issue")?;
    assert!(
        repo_issues.iter().any(|event| event.id == issue_id),
        "created issue did not reach the repository relay"
    );

    let repo_relay = RelayUrl::parse(&harness.grasp("repo").relay_url())?;
    let closed = EventBuilder::new(Kind::GitStatusClosed, "personal relay only")
        .tags(vec![
            Tag::from(nip10::Nip10Tag::Event {
                id: issue_id,
                relay_hint: Some(repo_relay.clone()),
                marker: Some(nip10::Marker::Root),
                public_key: None,
            }),
            Tag::from(nip01::Nip01Tag::Coordinate {
                coordinate: nip01::Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: published.maintainer_keys.public_key(),
                    identifier: published.identifier.clone(),
                },
                relay_hint: Some(repo_relay),
            }),
            Tag::public_key(published.maintainer_keys.public_key()),
            Tag::custom("r", vec![published.initial_oid.clone()]),
        ])
        .finalize(&published.maintainer_keys)?;

    publish_to_relay(harness.relay("default").url(), &closed).await?;

    let bare_url = format!(
        "nostr://{}/{}",
        published.maintainer_npub, published.identifier
    );
    let bare_clone = harness
        .clone_url(&bare_url)
        .await
        .context("clone repository from bare nostr URL")?;
    assert_eq!(
        issue_status(&bare_clone).await?,
        "open",
        "a fallback relay used to find the announcement must not supply repository status"
    );

    assert_eq!(
        issue_status(&publisher).await?,
        "open",
        "a status found only on a personal relay must not affect repository state"
    );

    publish_to_relay(harness.grasp("repo").relay_url(), &closed).await?;
    assert_eq!(
        issue_status(&publisher).await?,
        "closed",
        "the same status must be observed once the repository relay serves it"
    );

    Ok(())
}

async fn issue_status(repo: &test_harness::Repo) -> Result<String> {
    let output = repo
        .ngit(["issue", "list", "--status", "open,closed", "--json"])
        .output()
        .await
        .context("failed to spawn `ngit issue list --json`")?;
    assert_success("ngit issue list --json", &output)?;

    let rows: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("issue list stdout was not valid JSON")?;
    rows.as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["subject"] == "relay scoped issue")
        })
        .and_then(|row| row["status"].as_str())
        .map(str::to_string)
        .context("relay scoped issue was missing from JSON output")
}

async fn publish_to_relay(relay_url: impl AsRef<str>, event: &Event) -> Result<()> {
    let relay_url = relay_url.as_ref();
    let client = Client::default();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("add relay {relay_url}"))?;
    client.connect().await;
    let output = client
        .send_event(event)
        .to([relay_url])
        .await
        .with_context(|| format!("publish event {} to {relay_url}", event.id))?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "relay {relay_url} rejected event {}: {:?}",
            event.id,
            output.failed
        );
    }
    Ok(())
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [tag_name, value, ..] if tag_name == name => Some(value.as_str()),
        _ => None,
    })
}

fn assert_success(label: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    }
}
