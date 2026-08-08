//! End-to-end coverage for indexed non-default PR targets through both
//! user-facing creation paths.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, event_branch_name_tag, tag_value,
};

#[tokio::test]
async fn send_and_git_push_target_a_non_default_branch() -> Result<()> {
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
            identifier: Some("pr-target-test".into()),
            ..Default::default()
        })
        .await?;

    publisher
        .git_ok(["checkout", "-b", "release/2.x"], "create target branch")
        .await?;
    commit_file(&publisher, "release.md", "release\n", "release baseline").await?;
    publisher
        .nostr_push(["-u", "origin", "release/2.x"])
        .await?;
    let target_tip = publisher.rev_parse("release/2.x").await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "target contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "release-fix", "origin/release/2.x"],
            "branch from target",
        )
        .await?;
    commit_file(&contributor, "fix.md", "fix\n", "fix release").await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--target-branch",
            "release/2.x",
        ],
    )
    .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "pr/push-target", "origin/release/2.x"],
            "create pushed target PR",
        )
        .await?;
    commit_file(
        &contributor,
        "push-target.md",
        "push target\n",
        "push target change",
    )
    .await?;
    contributor
        .nostr_push([
            "-u",
            "origin",
            "pr/push-target",
            "-o",
            "target-branch=release/2.x",
        ])
        .await?;

    let targeted = find_pr(&harness, "release-fix").await?;
    assert_eq!(tag_value(&targeted, "b").as_deref(), Some("release/2.x"));
    assert_eq!(
        tag_value(&targeted, "merge-base").as_deref(),
        Some(target_tip.as_str())
    );
    let pushed_target = find_pr(&harness, "push-target").await?;
    assert_eq!(
        tag_value(&pushed_target, "b").as_deref(),
        Some("release/2.x")
    );

    let maintainer = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;
    ngit_ok(
        &maintainer,
        &["merge", &targeted.id.to_hex(), "--exclude-description"],
    )
    .await?;
    let branch = maintainer
        .git(["branch", "--show-current"])
        .output()
        .await?;
    assert!(branch.status.success());
    assert_eq!(String::from_utf8(branch.stdout)?.trim(), "release/2.x");
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

async fn ngit_ok(repo: &test_harness::Repo, args: &[&str]) -> Result<()> {
    let output = repo.ngit(args.iter().copied()).output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "ngit {args:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}
