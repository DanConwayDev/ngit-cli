//! NIP-42 relay authentication policy, end to end.
//!
//! ngit answers AUTH challenges from repository relays only after the command
//! has explicitly attached a signer. Known-private discovery requires that
//! acquisition; public operations never acquire credentials in response to a
//! challenge. The user's own inbox/outbox relays authenticate only while
//! publishing, and every unrelated relay is declined.
//!
//! These tests gate harness relays behind NIP-42 and assert observable
//! delivery and read behavior. Policy unit tests cover the no-signer and
//! unrelated-relay refusal paths without disclosing a test identity on the
//! wire.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{
    Harness, KIND_REPO_STATE, LocalRelayBuilderNip42, PublishPrOpts, PublishRepoOpts, tag_value,
};

#[tokio::test]
async fn publishes_to_repo_relay_that_requires_auth_to_write() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay_nip42("auth-repo", LocalRelayBuilderNip42::write())
    .with_grasp_server("repo")
    .build()
    .await?;

    let auth_repo_url = harness.relay("auth-repo").url().to_string();
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("nip42-write-repo-relay".into()),
            extra_repo_relays: vec![auth_repo_url],
            ..Default::default()
        })
        .await?;
    let maintainer = published.maintainer_keys.public_key();

    // The announcement (ngit init) and state event (push) can only have
    // landed on the write-gated repo relay by answering its AUTH
    // challenge. Reads on a write-gated relay are open, so the harness
    // can query it directly.
    let announcements = harness
        .relay("auth-repo")
        .events(
            Filter::new()
                .author(maintainer)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    assert!(
        !announcements.is_empty(),
        "kind-30617 announcement missing from the auth-to-write repo relay"
    );

    let states = harness
        .relay("auth-repo")
        .events(Filter::new().author(maintainer).kind(KIND_REPO_STATE))
        .await?;
    assert!(
        !states.is_empty(),
        "kind-30618 state event missing from the auth-to-write repo relay"
    );

    // A contributor's PR fans out to every repo relay, again requiring
    // auth on this one.
    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: Some("nip42-feature".into()),
                commits: vec![("nip42.md".into(), "auth-gated content\n".into())],
                title: "auth gated pr".into(),
                description: "published via NIP-42 auth".into(),
                in_reply_to: vec![],
            },
        )
        .await?;
    let prs = harness
        .relay("auth-repo")
        .events(Filter::new().id(pr.event_id))
        .await?;
    assert!(
        !prs.is_empty(),
        "kind-1618 PR event missing from the auth-to-write repo relay"
    );

    Ok(())
}

#[tokio::test]
async fn publishes_to_user_outbox_that_requires_auth_to_write() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay_nip42("default", LocalRelayBuilderNip42::write())
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("nip42-write-outbox".into()),
            ..Default::default()
        })
        .await?;
    let maintainer = published.maintainer_keys.public_key();

    // `ngit account create` publishes kind 0 + 10002 to the relays that
    // become the user's inbox/outbox — a publish, so ngit must have
    // answered the write-gated relay's AUTH challenge.
    let profiles = harness
        .relay("default")
        .events(Filter::new().author(maintainer).kind(Kind::Metadata))
        .await?;
    assert!(
        !profiles.is_empty(),
        "kind-0 profile missing from the auth-to-write outbox relay"
    );
    let relay_lists = harness
        .relay("default")
        .events(Filter::new().author(maintainer).kind(Kind::RelayList))
        .await?;
    assert!(
        !relay_lists.is_empty(),
        "kind-10002 relay list missing from the auth-to-write outbox relay"
    );

    // A PR publish fans out to the contributor's own outbox (the same
    // default set) as well as the repo relays.
    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: Some("nip42-outbox-feature".into()),
                commits: vec![("outbox.md".into(), "auth-gated outbox content\n".into())],
                title: "auth gated outbox pr".into(),
                description: "published via NIP-42 auth".into(),
                in_reply_to: vec![],
            },
        )
        .await?;
    let prs = harness
        .relay("default")
        .events(Filter::new().id(pr.event_id))
        .await?;
    assert!(
        !prs.is_empty(),
        "kind-1618 PR event missing from the auth-to-write outbox relay"
    );

    Ok(())
}

#[tokio::test]
async fn reads_repo_status_from_repo_relay_that_requires_auth_to_read() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay_nip42("auth-read", LocalRelayBuilderNip42::read())
    .with_grasp_server("repo")
    .build()
    .await?;

    let auth_read_url = harness.relay("auth-read").url().to_string();
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("nip42-read-repo-relay".into()),
            extra_repo_relays: vec![auth_read_url.clone()],
            ..Default::default()
        })
        .await?;
    let maintainer = published.maintainer_keys.public_key();

    let create = publisher
        .ngit([
            "issue",
            "create",
            "--subject",
            "auth gated issue",
            "--body",
            "body",
        ])
        .output()
        .await
        .context("failed to spawn `ngit issue create`")?;
    assert_success("ngit issue create", &create)?;

    let issues = harness
        .grasp("repo")
        .events(Filter::new().author(maintainer).kind(Kind::GitIssue))
        .await?;
    let issue = issues
        .iter()
        .find(|event| tag_value(event, "subject").as_deref() == Some("auth gated issue"))
        .context("created issue did not reach the repository relay")?;

    // Place the closed status only on the read-gated repo relay. Writes
    // to a read-gated relay are open, so a plain unauthenticated client
    // can seed it — but reading it back requires NIP-42 auth.
    let repo_relay = RelayUrl::parse(&harness.grasp("repo").relay_url())?;
    let closed = EventBuilder::new(Kind::GitStatusClosed, "auth-read relay only")
        .tags(vec![
            Tag::from(nip10::Nip10Tag::Event {
                id: issue.id,
                relay_hint: Some(repo_relay.clone()),
                marker: Some(nip10::Marker::Root),
                public_key: None,
            }),
            Tag::from(nip01::Nip01Tag::Coordinate {
                coordinate: nip01::Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: maintainer,
                    identifier: published.identifier.clone(),
                },
                relay_hint: Some(repo_relay),
            }),
            Tag::public_key(maintainer),
            Tag::custom("r", vec![published.initial_oid.clone()]),
        ])
        .finalize(&published.maintainer_keys)?;
    publish_to_relay(&auth_read_url, &closed).await?;

    // Listing issues reads collaboration events from the repo relays;
    // the closed status is only visible if ngit authenticated to the
    // read-gated relay.
    assert_eq!(
        issue_status(&publisher).await?,
        "closed",
        "status held only by an auth-to-read repo relay must be visible — \
         ngit should authenticate because this command attached its account signer"
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
        .and_then(|rows| rows.iter().find(|row| row["subject"] == "auth gated issue"))
        .and_then(|row| row["status"].as_str())
        .map(str::to_string)
        .context("auth gated issue was missing from JSON output")
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
