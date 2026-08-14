//! End-to-end GRASP-08 coverage against the pinned `ngit-grasp` subprocess.
//!
//! A private repository must authenticate its member at both service
//! boundaries: NIP-42 before relay traffic and the repository-scoped NIP-98
//! profile before Git Smart HTTP traffic. Repository events must remain on the
//! repository relay; only the encrypted account-level kind 10318 discovery
//! list belongs on the member's ordinary relays.

use anyhow::{Context, Result, bail};
use git2::Repository;
use nostr_sdk::prelude::*;
use tempfile::NamedTempFile;
use test_harness::Harness;

const KIND_PRIVATE_GIT_RELAY_LIST: Kind = Kind::Custom(10318);
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

async fn publish_member_relay_list(harness: &Harness, keys: &Keys) -> Result<()> {
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
    Ok(())
}

#[tokio::test]
async fn private_member_can_init_clone_and_push_without_public_repo_events() -> Result<()> {
    let member = Keys::generate();
    let member_nsec = member.secret_key().to_bech32()?;
    let member_pubkey = member.public_key();
    let member_npub = member_pubkey.to_bech32()?;
    let outsider_nsec = Keys::generate().secret_key().to_bech32()?;
    let credentials = NamedTempFile::new()?;

    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("inbox")
    .with_relay("outbox")
    .with_relay("blaster")
    .with_relay("signer_fallback")
    .with_private_grasp_server("repo", &member_pubkey)
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

    publish_member_relay_list(&harness, &member).await?;

    let publisher = harness.fresh_repo()?;
    let login = publisher
        .ngit([
            "account",
            "login",
            "--local",
            "--nsec",
            &member_nsec,
            "--alias",
            "member",
        ])
        .output()
        .await
        .context("failed to spawn member login")?;
    require_success("member login", &login)?;

    std::fs::write(
        publisher.dir().join("README.md"),
        "private GRASP-08 repository\n",
    )?;
    let add = publisher.git(["add", "README.md"]).output().await?;
    require_success("git add", &add)?;
    let commit = publisher
        .git(["commit", "-m", "initial", "--no-gpg-sign"])
        .output()
        .await?;
    require_success("git commit", &commit)?;
    let initial_oid = publisher
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("main ref missing after initial commit")?
        .clone();

    let identifier = "private-grasp08";
    let grasp_url = harness.grasp("repo").url().to_string();
    let init = publisher
        .ngit([
            "--signer",
            "member",
            "init",
            "--name",
            "private GRASP-08",
            "--identifier",
            identifier,
            "--grasp-server",
            &grasp_url,
            "--private",
            "--defaults",
        ])
        .output()
        .await
        .context("failed to spawn private ngit init")?;
    require_success("private ngit init", &init)?;

    let clone_url = publisher
        .config("remote.origin.url")
        .await?
        .context("ngit init did not configure origin")?;
    let bare_path = harness
        .grasp("repo")
        .git_data_path()
        .join(&member_npub)
        .join(format!("{identifier}.git"));
    let bare = Repository::open_bare(&bare_path)
        .with_context(|| format!("private bare repository missing at {}", bare_path.display()))?;
    assert_eq!(
        bare.refname_to_id("refs/heads/main")?.to_string(),
        initial_oid,
        "initial NIP-98 Git push did not reach the private service",
    );

    let private_events = harness
        .grasp("repo")
        .events_as(
            &member,
            Filter::new()
                .author(member_pubkey)
                .kinds([KIND_REPO_ANNOUNCEMENT, KIND_REPO_STATE]),
        )
        .await
        .context("member could not query the private service over NIP-42")?;
    assert!(
        private_events
            .iter()
            .any(|event| event.kind == KIND_REPO_ANNOUNCEMENT),
        "private service did not retain the repository announcement",
    );
    assert!(
        private_events
            .iter()
            .any(|event| event.kind == KIND_REPO_STATE),
        "private service did not retain the repository state",
    );
    let announcement = private_events
        .iter()
        .find(|event| event.kind == KIND_REPO_ANNOUNCEMENT)
        .context("private announcement missing")?;
    assert!(
        announcement
            .tags
            .iter()
            .any(|tag| { tag.as_slice() == ["private".to_string(), "true".to_string()] })
    );

    for role in ["default", "inbox", "outbox", "blaster", "signer_fallback"] {
        let leaked = harness
            .relay(role)
            .events(
                Filter::new()
                    .author(member_pubkey)
                    .kinds([KIND_REPO_ANNOUNCEMENT, KIND_REPO_STATE]),
            )
            .await?;
        assert!(
            leaked.is_empty(),
            "private repository events leaked to the {role} relay: {leaked:?}",
        );
    }

    let mut discovery_counts = Vec::new();
    for role in ["default", "inbox", "outbox", "blaster", "signer_fallback"] {
        let discovery = harness
            .relay(role)
            .events(
                Filter::new()
                    .author(member_pubkey)
                    .kind(KIND_PRIVATE_GIT_RELAY_LIST),
            )
            .await?;
        assert!(discovery.iter().all(|event| event.tags.is_empty()));
        discovery_counts.push((role, discovery.len()));
    }
    assert_eq!(
        discovery_counts,
        vec![
            ("default", 0),
            ("inbox", 0),
            ("outbox", 1),
            ("blaster", 0),
            ("signer_fallback", 0),
        ],
        "encrypted private relay discovery used unexpected ordinary relays",
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
        .clone_url_with_git_config(&clone_url, &[("nostr.signer", "outsider")])
        .await;
    if let Ok(outsider) = outsider {
        panic!(
            "a non-member unexpectedly cloned the private repo with refs: {:?}",
            outsider.snapshot()?.refs
        );
    }

    let member_clone = harness
        .clone_url_with_git_config(&clone_url, &[("nostr.signer", "member")])
        .await
        .context("member failed to clone the private repository")?;
    assert_eq!(
        member_clone
            .snapshot()?
            .refs
            .get("refs/heads/main")
            .context("member clone has no main ref")?,
        &initial_oid,
    );
    assert!(
        member_clone.config("nostr.signer").await?.is_none(),
        "command-scoped clone selection must not persist a configured signer",
    );
    let list = member_clone
        .ngit(["--signer", "member", "pr", "list"])
        .output()
        .await?;
    require_success("member PR list with one-shot signer", &list)?;

    std::fs::write(
        member_clone.dir().join("member.txt"),
        "authenticated member push\n",
    )?;
    require_success(
        "git add member.txt",
        &member_clone.git(["add", "member.txt"]).output().await?,
    )?;
    require_success(
        "git commit member update",
        &member_clone
            .git(["commit", "-m", "member update", "--no-gpg-sign"])
            .output()
            .await?,
    )?;
    member_clone
        .nostr_push_with_git_flags(["-c", "nostr.signer=member"], ["origin", "main"])
        .await?;
    let updated_oid = member_clone
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("member clone lost main ref")?
        .clone();
    assert_eq!(
        bare.refname_to_id("refs/heads/main")?.to_string(),
        updated_oid,
        "authenticated member update did not reach the private Git service",
    );

    for role in ["default", "inbox", "outbox", "blaster", "signer_fallback"] {
        assert!(
            harness
                .relay(role)
                .events(
                    Filter::new()
                        .author(member_pubkey)
                        .kinds([KIND_REPO_ANNOUNCEMENT, KIND_REPO_STATE]),
                )
                .await?
                .is_empty(),
            "member push leaked repository events to the {role} relay",
        );
    }

    Ok(())
}
