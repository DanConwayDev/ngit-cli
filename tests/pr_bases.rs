//! End-to-end coverage for explicit proposal bases through both user-facing
//! creation paths.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, event_branch_name_tag, tag_value,
};

#[tokio::test]
async fn root_and_update_references_select_the_expected_base() -> Result<()> {
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
            identifier: Some("pr-base-test".into()),
            ..Default::default()
        })
        .await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "base contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(["checkout", "-b", "pr/parent"], "create parent PR")
        .await?;
    commit_file(&contributor, "parent.md", "parent\n", "parent change").await?;
    contributor
        .nostr_push(["-u", "origin", "pr/parent"])
        .await?;

    let parent = find_pr(&harness, "parent").await?;
    commit_file(
        &contributor,
        "parent-update-one.md",
        "first update\n",
        "first parent update",
    )
    .await?;
    let historical_tip = contributor.rev_parse("HEAD").await?;
    contributor.nostr_push(["origin", "pr/parent"]).await?;
    let historical_update = find_pr_update_at(&harness, &historical_tip).await?;

    commit_file(
        &contributor,
        "parent-update-two.md",
        "second update\n",
        "second parent update",
    )
    .await?;
    let latest_parent_tip = contributor.rev_parse("HEAD").await?;
    contributor.nostr_push(["origin", "pr/parent"]).await?;

    contributor
        .git_ok(
            ["checkout", "-b", "send-child", &historical_tip],
            "create sent child PR from historical update",
        )
        .await?;
    commit_file(
        &contributor,
        "send-child.md",
        "sent child\n",
        "sent child change",
    )
    .await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--base",
            &format!("#{}", &historical_update.id.to_hex()[..8]),
        ],
    )
    .await?;
    contributor
        .git_ok(["checkout", "pr/parent"], "return to parent PR")
        .await?;
    contributor
        .git_ok(["checkout", "-b", "pr/child"], "create child PR")
        .await?;
    commit_file(&contributor, "child.md", "child\n", "child change").await?;
    contributor
        .nostr_push([
            "-u",
            "origin",
            "pr/child",
            "-o",
            &format!("base={}", parent.id.to_hex()),
        ])
        .await?;

    let child = find_pr(&harness, "child").await?;
    assert_eq!(
        tag_value(&child, "merge-base").as_deref(),
        Some(latest_parent_tip.as_str())
    );
    assert!(tag_value(&child, "s").is_none());
    assert!(tag_value(&child, "b").is_none());
    let sent_child = find_pr(&harness, "send-child").await?;
    assert_eq!(
        tag_value(&sent_child, "merge-base").as_deref(),
        Some(historical_tip.as_str())
    );
    assert!(tag_value(&sent_child, "s").is_none());

    contributor
        .git_ok(["checkout", "pr/child"], "check out pushed child")
        .await?;
    commit_file(
        &contributor,
        "child-update.md",
        "child update\n",
        "update pushed child",
    )
    .await?;
    let child_update_tip = contributor.rev_parse("HEAD").await?;
    contributor.nostr_push(["origin", "pr/child"]).await?;

    contributor
        .git_ok(["checkout", "send-child"], "check out sent child")
        .await?;
    commit_file(
        &contributor,
        "send-child-update.md",
        "sent update\n",
        "update sent child",
    )
    .await?;
    let sent_child_update_tip = contributor.rev_parse("HEAD").await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &sent_child.id.to_hex(),
        ],
    )
    .await?;

    assert_eq!(
        tag_value(
            &find_pr_update_at(&harness, &child_update_tip).await?,
            "merge-base"
        )
        .as_deref(),
        Some(latest_parent_tip.as_str())
    );
    assert_eq!(
        tag_value(
            &find_pr_update_at(&harness, &sent_child_update_tip).await?,
            "merge-base"
        )
        .as_deref(),
        Some(historical_tip.as_str())
    );

    contributor
        .git_ok(
            ["checkout", "-B", "pr/child", &latest_parent_tip],
            "rewrite pushed child from selected base",
        )
        .await?;
    commit_file(
        &contributor,
        "rewritten-child.md",
        "rewritten child\n",
        "rewrite pushed child",
    )
    .await?;
    let rewritten_child_tip = contributor.rev_parse("HEAD").await?;
    contributor
        .nostr_push([
            "--force",
            "origin",
            "pr/child",
            "-o",
            &format!("base={}", parent.id.to_hex()),
        ])
        .await?;

    contributor
        .git_ok(
            ["checkout", "-B", "send-child", &historical_tip],
            "rewrite sent child from selected base",
        )
        .await?;
    commit_file(
        &contributor,
        "rewritten-send-child.md",
        "rewritten sent child\n",
        "rewrite sent child",
    )
    .await?;
    let rewritten_sent_tip = contributor.rev_parse("HEAD").await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &sent_child.id.to_hex(),
            "--base",
            &historical_update.id.to_hex(),
        ],
    )
    .await?;

    assert_eq!(
        tag_value(
            &find_pr_update_at(&harness, &rewritten_child_tip).await?,
            "merge-base"
        )
        .as_deref(),
        Some(latest_parent_tip.as_str())
    );
    assert_eq!(
        tag_value(
            &find_pr_update_at(&harness, &rewritten_sent_tip).await?,
            "merge-base"
        )
        .as_deref(),
        Some(historical_tip.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn invalid_bases_fail_through_send_and_git_push_without_events() -> Result<()> {
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
            identifier: Some("pr-base-errors-test".into()),
            ..Default::default()
        })
        .await?;
    publisher
        .git_ok(["checkout", "-b", "release/2.x"], "create nested base")
        .await?;
    commit_file(
        &publisher,
        "nested-base.md",
        "nested base\n",
        "add nested base",
    )
    .await?;
    publisher
        .nostr_push(["-u", "origin", "release/2.x"])
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "base error contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(["checkout", "-b", "pr/base-errors"], "create error PR")
        .await?;
    commit_file(&contributor, "error.md", "error\n", "error change").await?;

    for base in ["HEAD", "missing-base", "#00000000", "2.x"] {
        ngit_fails(
            &contributor,
            &["send", "--defaults", "--force-pr", "--base", base],
        )
        .await?;
        contributor
            .nostr_push_expecting_failure([
                "origin",
                "pr/base-errors",
                "-o",
                &format!("base={base}"),
            ])
            .await?;
    }
    for incompatible in ["--force-patch", "--no-cover-letter"] {
        ngit_fails(
            &contributor,
            &["send", "--defaults", "--base", "main", incompatible],
        )
        .await?;
    }

    assert!(
        harness
            .grasp("repo")
            .events(Filter::new().kind(KIND_PULL_REQUEST))
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn patch_to_pr_upgrade_roots_are_valid_bases_through_both_surfaces() -> Result<()> {
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
            identifier: Some("pr-upgrade-base-test".into()),
            ..Default::default()
        })
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "upgrade base contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(["checkout", "-b", "upgrade-parent"], "create patch parent")
        .await?;
    commit_file(
        &contributor,
        "upgrade-parent.md",
        "upgrade parent\n",
        "upgrade parent",
    )
    .await?;
    let upgrade_tip = contributor.rev_parse("HEAD").await?;
    ngit_ok(&contributor, &["send", "--defaults", "--force-patch"]).await?;
    let patch_root = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitPatch))
        .await?
        .into_iter()
        .find(|event| event_branch_name_tag(event).as_deref() == Some("upgrade-parent"))
        .context("missing patch proposal root")?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &patch_root.id.to_hex(),
        ],
    )
    .await?;
    let upgrade_root = find_pr(&harness, "upgrade-parent").await?;
    let upgrade_nevent = Nip19Event {
        event_id: upgrade_root.id,
        relays: vec![],
        author: Some(upgrade_root.pubkey),
        kind: Some(upgrade_root.kind),
    }
    .to_bech32()?;

    contributor
        .git_ok(
            ["checkout", "-b", "pr/upgrade-push", &upgrade_tip],
            "create pushed child from upgrade root",
        )
        .await?;
    commit_file(
        &contributor,
        "upgrade-push.md",
        "upgrade push\n",
        "upgrade push child",
    )
    .await?;
    contributor
        .nostr_push([
            "-u",
            "origin",
            "pr/upgrade-push",
            "-o",
            &format!("base={}", upgrade_root.id.to_hex()),
        ])
        .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "upgrade-send", &upgrade_tip],
            "create sent child from upgrade root",
        )
        .await?;
    commit_file(
        &contributor,
        "upgrade-send.md",
        "upgrade send\n",
        "upgrade send child",
    )
    .await?;
    ngit_ok(
        &contributor,
        &["send", "--defaults", "--base", &upgrade_nevent],
    )
    .await?;

    for proposal in [
        find_pr(&harness, "upgrade-push").await?,
        find_pr(&harness, "upgrade-send").await?,
    ] {
        assert_eq!(
            tag_value(&proposal, "merge-base").as_deref(),
            Some(upgrade_tip.as_str())
        );
    }
    Ok(())
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
