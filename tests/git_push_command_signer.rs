//! End-to-end coverage for one-shot signer selection on Git commands via
//! `git -c nostr.signer=<selector>`, read by the remote helper from Git's
//! command-scoped config.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use tempfile::NamedTempFile;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, event_branch_name_tag,
};

#[tokio::test]
async fn command_scoped_signer_signs_the_push_as_the_selected_identity() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("command-signer-test".into()),
            ..Default::default()
        })
        .await?;

    let mut contributor = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    // Route stored secrets through ngit's file store so the remote helper
    // spawned by `git push` can resolve selections; the harness default is
    // plaintext git-config storage, which holds only the active login.
    let credentials = NamedTempFile::new()?;
    contributor.set_env("NGIT_SECRET_STORAGE", "file");
    contributor.set_env(
        "NGIT_KEYRING_FILE",
        credentials
            .path()
            .to_str()
            .context("keyring path is not utf-8")?,
    );

    // the selectable identity: stored credentials plus a cached kind-0
    // profile name
    let output = contributor
        .ngit([
            "account",
            "create",
            "--local",
            "--name",
            "Command Signer Bob",
        ])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "account create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let selected_npub = contributor
        .config("nostr.npub")
        .await?
        .context("nostr.npub missing after account create")?;

    // a different identity becomes (and must remain) the configured login
    let login_keys = Keys::generate();
    let output = contributor
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &login_keys.secret_key().to_bech32()?,
        ])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "account login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let login_npub = login_keys.public_key().to_bech32()?;
    assert_eq!(
        contributor.config("nostr.npub").await?.as_deref(),
        Some(login_npub.as_str())
    );
    assert_ne!(selected_npub, login_npub);

    // baseline: a plain push signs as the configured login
    contributor
        .git_ok(
            ["checkout", "-b", "pr/default-identity"],
            "create default-identity PR branch",
        )
        .await?;
    commit_file(&contributor, "default.md", "default\n", "default identity").await?;
    contributor
        .nostr_push(["-u", "origin", "pr/default-identity"])
        .await?;
    assert_eq!(
        find_pr(&harness, "default-identity")
            .await?
            .pubkey
            .to_bech32()?,
        login_npub
    );

    // `-c nostr.signer=<name>` selects the other stored identity for this
    // push only, matching the cached profile name case-insensitively
    contributor
        .git_ok(
            ["checkout", "-b", "pr/command-signer", "main"],
            "create command-signer PR branch",
        )
        .await?;
    commit_file(&contributor, "selected.md", "selected\n", "command signer").await?;
    contributor
        .nostr_push_with_git_flags(
            ["-c", "nostr.signer=command signer bob"],
            ["-u", "origin", "pr/command-signer"],
        )
        .await?;
    assert_eq!(
        find_pr(&harness, "command-signer")
            .await?
            .pubkey
            .to_bech32()?,
        selected_npub
    );

    // the one-shot selection persisted nothing
    assert_eq!(
        contributor.config("nostr.npub").await?.as_deref(),
        Some(login_npub.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn unresolvable_command_signer_fails_closed() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("command-signer-fail-test".into()),
            ..Default::default()
        })
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "configured login".into(),
            },
        )
        .await?;

    // an explicit but unresolvable selection must fail the fetch rather
    // than silently degrading to anonymous relay access
    let output = contributor
        .git(["-c", "nostr.signer=No Such Person", "fetch", "origin"])
        .output()
        .await?;
    anyhow::ensure!(
        !output.status.success(),
        "fetch with an unresolvable signer unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nostr.signer"),
        "fetch error must name the command-scoped selector: {stderr}"
    );

    // without a selection the same fetch still works
    contributor
        .git_ok(["fetch", "origin"], "default fetch")
        .await?;

    // a push with the same selector fails before publishing anything
    contributor
        .git_ok(
            ["checkout", "-b", "pr/never-published"],
            "create never-published PR branch",
        )
        .await?;
    commit_file(&contributor, "never.md", "never\n", "never published").await?;
    contributor
        .nostr_push_with_git_flags_expecting_failure(
            ["-c", "nostr.signer=No Such Person"],
            ["-u", "origin", "pr/never-published"],
        )
        .await?;
    assert!(
        harness
            .grasp("repo")
            .events(Filter::new().kind(KIND_PULL_REQUEST))
            .await?
            .iter()
            .all(|event| event_branch_name_tag(event).as_deref() != Some("never-published")),
        "a failed push must not publish a PR event"
    );
    Ok(())
}

async fn find_pr(harness: &Harness, branch: &str) -> Result<Event> {
    harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST))
        .await?
        .into_iter()
        .find(|event| event_branch_name_tag(event).as_deref() == Some(branch))
        .with_context(|| format!("missing PR event for branch {branch}"))
}

async fn commit_file(
    repo: &test_harness::Repo,
    filename: &str,
    content: &str,
    message: &str,
) -> Result<()> {
    std::fs::write(repo.dir().join(filename), content)?;
    repo.git_ok(["add", filename], "stage test commit").await?;
    repo.git_ok(
        ["commit", "-m", message, "--no-gpg-sign"],
        "create test commit",
    )
    .await
}
