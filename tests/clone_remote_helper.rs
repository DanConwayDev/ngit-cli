//! End-to-end coverage for custom Git remote helpers behind a `nostr://`
//! remote.
//!
//! The backing repository is exposed only as an `ext::` URL. The test then
//! runs the production process chain:
//!
//! ```text
//! git clone nostr://... -> git-remote-nostr -> git -> git-remote-ext
//! ```
//!
//! libgit2 cannot parse the `ext::` URL, so reproducing the committed tree in
//! the fresh clone proves that ngit's Git-delegated list and fetch paths were
//! used. `git-remote-ext` ships with Git and exercises its `connect`
//! capability family without adding an external test dependency.

use anyhow::{Context, Result, ensure};
use nostr_sdk::prelude::*;
use test_harness::Harness;

#[tokio::test]
async fn git_clone_and_push_nostr_url_through_external_remote_helper() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let publisher = harness.fresh_repo()?;
    let filename = "REMOTE_HELPER_E2E.md";
    let contents = "cloned through git-remote-nostr and git-remote-ext\n";
    std::fs::write(publisher.dir().join(filename), contents).context("write seed file")?;
    require_success(
        "git add",
        &publisher
            .git(["add", filename])
            .output()
            .await
            .context("spawn git add")?,
    )?;
    require_success(
        "git commit",
        &publisher
            .git(["commit", "-m", "seed remote-helper e2e", "--no-gpg-sign"])
            .output()
            .await
            .context("spawn git commit")?,
    )?;

    let source = publisher.snapshot()?;
    let main_oid = source
        .refs
        .get("refs/heads/main")
        .context("publisher main ref missing")?
        .clone();

    let keys = Keys::generate();
    let pubkey = keys.public_key();
    let pubkey_hex = pubkey.to_string();
    let npub = pubkey.to_bech32().context("encode publisher npub")?;
    let identifier = "remote-helper-cli-e2e";
    let relay_url = harness.relay("default").url().to_string();
    let backing_dir = tempfile::tempdir().context("allocate backing repo directory")?;
    let backing_repo = backing_dir.path().join("backing.git");
    let backing_repo_str = backing_repo
        .to_str()
        .context("backing repository path is not UTF-8")?;
    require_success(
        "git init --bare backing repository",
        &publisher
            .git(["init", "--bare", backing_repo_str])
            .output()
            .await
            .context("spawn git init --bare")?,
    )?;
    require_success(
        "seed backing repository",
        &publisher
            .git(["push", backing_repo_str, "main:refs/heads/main"])
            .output()
            .await
            .context("spawn seed push")?,
    )?;
    require_success(
        "set backing repository HEAD",
        &publisher
            .git([
                "--git-dir",
                backing_repo_str,
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ])
            .output()
            .await
            .context("spawn git symbolic-ref")?,
    )?;
    let helper_url = format!("ext::%S {backing_repo_str}");

    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(vec![
            Tag::identifier(identifier.to_string()),
            Tag::parse(["r", main_oid.as_str(), "euc"])?,
            Tag::parse(["name", "remote helper CLI e2e"])?,
            Tag::parse(["description", "exercises nested Git remote helpers"])?,
            Tag::parse(["clone", helper_url.as_str()])?,
            Tag::parse(["relays", relay_url.as_str()])?,
            Tag::parse(["maintainers", pubkey_hex.as_str()])?,
            Tag::parse(["alt", "git repository: remote helper CLI e2e"])?,
        ])
        .finalize(&keys)?;
    publish_event(&announcement, &relay_url).await?;

    let relay_hint = urlencoding::encode(&relay_url);
    let nostr_url = format!("nostr://{npub}/{relay_hint}/{identifier}");
    let cloned = harness
        .clone_url_with_git_config(&nostr_url, &[("protocol.ext.allow", "always")])
        .await?;
    let cloned_snapshot = cloned.snapshot()?;
    assert_eq!(cloned_snapshot.head.as_deref(), Some("refs/heads/main"));
    assert_eq!(
        cloned_snapshot
            .refs
            .get("refs/heads/main")
            .map(String::as_str),
        Some(main_oid.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(cloned.dir().join(filename))?,
        contents
    );

    let nsec = keys.secret_key().to_bech32()?;
    cloned
        .git_ok(
            ["config", "--local", "nostr.nsec", &nsec],
            "git config nostr.nsec",
        )
        .await?;
    cloned
        .git_ok(
            ["config", "--local", "protocol.ext.allow", "always"],
            "git config protocol.ext.allow",
        )
        .await?;
    let pushed_filename = "PUSHED_THROUGH_HELPER.md";
    std::fs::write(
        cloned.dir().join(pushed_filename),
        "pushed through both remote helpers\n",
    )?;
    cloned
        .git_ok(["add", pushed_filename], "git add pushed file")
        .await?;
    cloned
        .git_ok(
            ["commit", "-m", "push through helper", "--no-gpg-sign"],
            "git commit pushed file",
        )
        .await?;
    let pushed_oid = cloned.rev_parse("HEAD").await?;
    cloned.nostr_push(["origin", "main"]).await?;

    let backing_oid = publisher
        .git([
            "--git-dir",
            backing_repo_str,
            "rev-parse",
            "refs/heads/main",
        ])
        .output()
        .await
        .context("read backing main after nostr push")?;
    require_success("read backing main after nostr push", &backing_oid)?;
    assert_eq!(
        String::from_utf8(backing_oid.stdout)?.trim(),
        pushed_oid,
        "the actual CLI push must advance the external-helper backing ref"
    );
    Ok(())
}

async fn publish_event(event: &Event, relay_url: &str) -> Result<()> {
    let client = Client::default();
    client.add_relay(relay_url).await?;
    client.connect().await;
    let output = client.send_event(event).await?;
    client.disconnect().await;
    ensure!(
        output.failed.is_empty(),
        "relay did not acknowledge announcement: {:?}",
        output.failed
    );
    Ok(())
}

fn require_success(label: &str, output: &std::process::Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}
