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
    assert_eq!(
        tag_value(&pushed_target, "merge-base").as_deref(),
        Some(target_tip.as_str())
    );

    contributor
        .git_ok(["checkout", "pr/push-target"], "check out pushed target PR")
        .await?;
    commit_file(
        &contributor,
        "push-target-update.md",
        "push update\n",
        "update pushed target",
    )
    .await?;
    let pushed_update_tip = contributor.rev_parse("HEAD").await?;
    contributor.nostr_push(["origin", "pr/push-target"]).await?;

    contributor
        .git_ok(["checkout", "release-fix"], "check out sent target PR")
        .await?;
    commit_file(
        &contributor,
        "send-target-update.md",
        "send update\n",
        "update sent target",
    )
    .await?;
    let sent_update_tip = contributor.rev_parse("HEAD").await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &targeted.id.to_hex(),
        ],
    )
    .await?;

    for update_tip in [&pushed_update_tip, &sent_update_tip] {
        let update = find_pr_update_at(&harness, update_tip).await?;
        assert_eq!(
            tag_value(&update, "merge-base").as_deref(),
            Some(target_tip.as_str())
        );
        assert!(tag_value(&update, "b").is_none());
    }

    let maintainer = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;
    maintainer
        .git_ok(["remote", "rename", "origin", "upstream"], "rename remote")
        .await?;
    let main_before_merge = maintainer.rev_parse("main").await?;
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
    assert_eq!(maintainer.rev_parse("main").await?, main_before_merge);
    maintainer.nostr_push(["upstream", "release/2.x"]).await?;
    let applied = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitStatusApplied))
        .await?;
    assert!(
        applied
            .iter()
            .any(|event| event_root_e_tag(event) == Some(targeted.id))
    );
    Ok(())
}

#[tokio::test]
async fn send_uses_advanced_remote_target_when_local_target_is_stale() -> Result<()> {
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
            identifier: Some("pr-stale-target-test".into()),
            ..Default::default()
        })
        .await?;

    publisher
        .git_ok(["checkout", "-b", "release/2.x"], "create target branch")
        .await?;
    commit_file(&publisher, "release-one.md", "one\n", "release one").await?;
    publisher
        .nostr_push(["-u", "origin", "release/2.x"])
        .await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "stale target contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(
            ["branch", "release/2.x", "origin/release/2.x"],
            "create local target copy",
        )
        .await?;

    commit_file(&publisher, "release-two.md", "two\n", "release two").await?;
    publisher.nostr_push(["origin", "release/2.x"]).await?;
    let advanced_target = publisher.rev_parse("release/2.x").await?;
    contributor
        .git_ok(["fetch", "origin"], "refresh remote target")
        .await?;
    contributor
        .git_ok(
            ["checkout", "-b", "stale-target-fix", "origin/release/2.x"],
            "branch from advanced target",
        )
        .await?;
    commit_file(&contributor, "fix.md", "fix\n", "target fix").await?;

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

    let proposal = find_pr(&harness, "stale-target-fix").await?;
    assert_eq!(
        tag_value(&proposal, "merge-base").as_deref(),
        Some(advanced_target.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn target_errors_are_rejected_by_send_and_git_push_without_events() -> Result<()> {
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
            identifier: Some("pr-target-errors-test".into()),
            ..Default::default()
        })
        .await?;
    publisher
        .git_ok(["checkout", "-b", "release/2.x"], "create nested target")
        .await?;
    commit_file(
        &publisher,
        "nested-target.md",
        "nested target\n",
        "add nested target",
    )
    .await?;
    publisher
        .nostr_push(["-u", "origin", "release/2.x"])
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "target error contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(["checkout", "-b", "pr/target-errors"], "create error PR")
        .await?;
    commit_file(&contributor, "error.md", "error\n", "error change").await?;

    for target in ["missing", "main", "bad..target", "2.x"] {
        ngit_fails(
            &contributor,
            &[
                "send",
                "--defaults",
                "--force-pr",
                "--target-branch",
                target,
            ],
        )
        .await?;
        contributor
            .nostr_push_expecting_failure([
                "origin",
                "pr/target-errors",
                "-o",
                &format!("target-branch={target}"),
            ])
            .await?;
    }
    ngit_fails(
        &contributor,
        &[
            "send",
            "--defaults",
            "--target-branch",
            "missing",
            "--force-patch",
        ],
    )
    .await?;

    assert!(
        harness
            .grasp("repo")
            .events(Filter::new().kind(KIND_PULL_REQUEST))
            .await?
            .is_empty()
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

async fn find_pr_update_at(harness: &Harness, commit: &str) -> Result<Event> {
    harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::Custom(1619)))
        .await?
        .into_iter()
        .find(|event| tag_value(event, "c").as_deref() == Some(commit))
        .with_context(|| format!("missing PR update at commit {commit}"))
}

fn event_root_e_tag(event: &Event) -> Option<EventId> {
    event.tags.iter().find_map(|tag| {
        let values = tag.as_slice();
        if values.first().map(String::as_str) != Some("e")
            || !values.iter().any(|value| value == "root")
        {
            return None;
        }
        values
            .get(1)
            .and_then(|value| EventId::from_hex(value).ok())
    })
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

async fn ngit_fails(repo: &test_harness::Repo, args: &[&str]) -> Result<()> {
    let output = repo.ngit(args.iter().copied()).output().await?;
    if output.status.success() {
        anyhow::bail!(
            "ngit {args:?} succeeded unexpectedly\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}
