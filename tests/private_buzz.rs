//! End-to-end private repository coverage against the pinned Buzz Nix build.
//!
//! Buzz uses NIP-42 for relay authentication, a `buzz-channel` tag as the
//! repository ACL, and repository-scoped NIP-98 for Git Smart HTTP. ngit must
//! preserve that announcement metadata, authenticate Git operations, and keep
//! repository events on the Buzz repository relay rather than the user's
//! ordinary inbox, outbox, blaster, or signer-fallback relays.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use tempfile::NamedTempFile;
use test_harness::{BuzzServer, Harness};

const KIND_REPO_ANNOUNCEMENT: Kind = Kind::Custom(30617);
const KIND_REPO_STATE: Kind = Kind::Custom(30618);

fn require_success(label: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

async fn publish_member_relay_lists(
    harness: &Harness,
    keys: &Keys,
    private_git_relay: &str,
) -> Result<()> {
    let bootstrap = harness.relay("default").url();
    let relay_list = RelayList::new([
        (
            RelayUrl::parse(harness.relay("inbox").url())?,
            Some(RelayMetadata::Read),
        ),
        (
            RelayUrl::parse(harness.relay("outbox").url())?,
            Some(RelayMetadata::Write),
        ),
    ])
    .finalize(keys)
    .context("failed to sign member relay list")?;

    let client = Client::default();
    client.add_relay(bootstrap).await?;
    client.connect().await;
    let output = client
        .send_event(&relay_list)
        .to([bootstrap])
        .await
        .context("failed to publish member relay list")?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "bootstrap relay rejected member relay list: {:?}",
            output.failed
        );
    }

    let signer = Arc::new(ngit::NgitSigner::Keys(keys.clone()));
    let private_list =
        ngit::login::user::PrivateGitRelayList::new(vec![RelayUrl::parse(private_git_relay)?])?
            .to_event(&signer)
            .await?;
    let outbox = harness.relay("outbox").url();
    client.add_relay(outbox).await?;
    client.connect().await;
    let output = client
        .send_event(&private_list)
        .to([outbox])
        .await
        .context("failed to publish encrypted private Git relay list")?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "outbox relay rejected private Git relay list: {:?}",
            output.failed
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ngit_clones_and_pushes_a_buzz_channel_repo_without_public_fanout() -> Result<()> {
    let owner = Keys::generate();
    let owner_nsec = owner.secret_key().to_bech32()?;
    let owner_npub = owner.public_key().to_bech32()?;
    let outsider_nsec = Keys::generate().secret_key().to_bech32()?;
    let credentials = NamedTempFile::new()?;

    let Some(buzz) = BuzzServer::start(&owner).await? else {
        // The pinned Buzz binary is unavailable and CI is unset; the fixture
        // already printed why. Treat as skipped rather than failed.
        return Ok(());
    };
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("inbox")
    .with_relay("outbox")
    .with_relay("blaster")
    .with_relay("signer_fallback")
    .with_child_env("NGIT_SECRET_STORAGE", "file")
    .with_child_env(
        "NGIT_KEYRING_FILE",
        credentials
            .path()
            .to_str()
            .context("credential file path is not UTF-8")?,
    )
    .build()
    .await?;
    publish_member_relay_lists(&harness, &owner, buzz.relay_url()).await?;

    let channel = buzz.create_private_channel("ngit-integration").await?;
    let identifier = "private-buzz";
    let clone_url = buzz.announce_repository(identifier, &channel).await?;
    let relay_hint = urlencoding::encode(buzz.relay_url()).into_owned();
    let nostr_url = format!("nostr://{owner_npub}/{relay_hint}/{identifier}");

    let publisher = harness.fresh_repo()?;
    let login = publisher
        .ngit([
            "account",
            "login",
            "--local",
            "--nsec",
            &owner_nsec,
            "--alias",
            "owner",
        ])
        .output()
        .await
        .context("failed to spawn publisher login")?;
    require_success("publisher login", &login)?;

    std::fs::write(
        publisher.dir().join("README.md"),
        "private repository hosted by Buzz\n",
    )?;
    require_success(
        "git add README.md",
        &publisher.git(["add", "README.md"]).output().await?,
    )?;
    require_success(
        "git commit",
        &publisher
            .git(["commit", "-m", "initial", "--no-gpg-sign"])
            .output()
            .await?,
    )?;
    require_success(
        "git remote add origin",
        &publisher
            .git(["remote", "add", "origin", &nostr_url])
            .output()
            .await?,
    )?;
    publisher
        .nostr_push_with_git_flags(["-c", "nostr.signer=owner"], ["-u", "origin", "main"])
        .await
        .context("initial ngit-backed push to Buzz failed")?;
    let initial_oid = publisher
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("publisher repository has no main ref after push")?
        .clone();
    let Err(unauthenticated_error) = harness.clone_url(&nostr_url).await else {
        bail!("copied Buzz URL cloned without a signer");
    };
    let unauthenticated_message = format!("{unauthenticated_error:#}").to_lowercase();
    assert!(
        unauthenticated_message.contains("logged-in account"),
        "a copied Buzz URL was not classified as private from NIP-11: {unauthenticated_error:#}",
    );
    let outsider = harness.fresh_repo()?;
    let outsider_login = outsider
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &outsider_nsec,
            "--alias",
            "outsider",
        ])
        .output()
        .await?;
    require_success("outsider login", &outsider_login)?;
    let outsider = harness
        .clone_url_with_git_config(&nostr_url, &[("nostr.signer", "outsider")])
        .await;
    assert!(
        outsider.is_err(),
        "a non-channel-member unexpectedly cloned the Buzz repository"
    );

    let member = harness
        .clone_url_with_git_config(&nostr_url, &[("nostr.signer", "owner")])
        .await
        .context("channel member failed to clone the Buzz repository")?;
    assert_eq!(
        member
            .snapshot()?
            .refs
            .get("refs/heads/main")
            .context("member Buzz clone has no main ref")?,
        &initial_oid,
        "Buzz did not round-trip the initial ngit-pushed Git state",
    );

    std::fs::write(member.dir().join("member.txt"), "member update via ngit\n")?;
    require_success(
        "git add member.txt",
        &member.git(["add", "member.txt"]).output().await?,
    )?;
    require_success(
        "git commit member update",
        &member
            .git(["commit", "-m", "member update", "--no-gpg-sign"])
            .output()
            .await?,
    )?;
    member
        .nostr_push_with_git_flags(["-c", "nostr.signer=owner"], ["origin", "main"])
        .await
        .context("member ngit-backed update to Buzz failed")?;
    let updated_oid = member
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("member repository has no main ref after update")?
        .clone();

    let second_clone = harness
        .clone_url_with_git_config(&nostr_url, &[("nostr.signer", "owner")])
        .await
        .context("member could not clone the pushed Buzz repository")?;
    assert_eq!(
        second_clone
            .snapshot()?
            .refs
            .get("refs/heads/main")
            .context("second Buzz clone has no main ref")?,
        &updated_oid,
        "Buzz did not round-trip the ngit-pushed member update",
    );

    let repo_events = buzz
        .events_as(
            &owner,
            Filter::new()
                .author(owner.public_key())
                .kinds([KIND_REPO_ANNOUNCEMENT, KIND_REPO_STATE]),
        )
        .await?;
    let announcement = repo_events
        .iter()
        .find(|event| event.kind == KIND_REPO_ANNOUNCEMENT)
        .context("Buzz repository announcement is missing")?;
    assert!(
        announcement
            .tags
            .iter()
            .any(|tag| { tag.as_slice() == ["buzz-channel".to_string(), channel.clone()] }),
        "ngit/Buzz round trip lost the buzz-channel ACL tag",
    );
    assert!(
        announcement
            .tags
            .iter()
            .any(|tag| { tag.as_slice() == ["clone".to_string(), clone_url.clone()] }),
        "Buzz announcement did not retain its authenticated clone URL",
    );
    assert!(
        repo_events
            .iter()
            .any(|event| event.kind == KIND_REPO_STATE),
        "ngit push did not publish repository state to the Buzz relay",
    );

    for role in ["default", "inbox", "outbox", "blaster", "signer_fallback"] {
        let leaked = harness
            .relay(role)
            .events(
                Filter::new()
                    .author(owner.public_key())
                    .kinds([KIND_REPO_ANNOUNCEMENT, KIND_REPO_STATE]),
            )
            .await?;
        assert!(
            leaked.is_empty(),
            "Buzz repository events leaked to the {role} relay: {leaked:?}",
        );
    }

    Ok(())
}
