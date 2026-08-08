//! End-to-end coverage for inferred and explicit proposal bases through both
//! user-facing creation paths.

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
            "--base",
            &historical_update.id.to_hex(),
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
async fn stack_bases_follow_parent_updates_through_send_and_git_push() -> Result<()> {
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
            identifier: Some("inferred-pr-base-test".into()),
            ..Default::default()
        })
        .await?;
    publisher
        .git_ok(["checkout", "-b", "release/2.x"], "create release target")
        .await?;
    commit_file(
        &publisher,
        "release.md",
        "release\n",
        "create release target",
    )
    .await?;
    publisher
        .nostr_push(["-u", "origin", "release/2.x"])
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "stack contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "pr/parent", "origin/release/2.x"],
            "create parent PR",
        )
        .await?;
    commit_file(&contributor, "parent-one.md", "one\n", "parent one").await?;
    let parent_one = contributor.rev_parse("HEAD").await?;
    contributor
        .nostr_push([
            "-u",
            "origin",
            "pr/parent",
            "-o",
            "target-branch=release/2.x",
        ])
        .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "pr/pushed-child", &parent_one],
            "create pushed child",
        )
        .await?;
    commit_file(
        &contributor,
        "pushed-child-one.md",
        "child one\n",
        "pushed child one",
    )
    .await?;
    contributor
        .nostr_push([
            "-u",
            "origin",
            "pr/pushed-child",
            "-o",
            "target-branch=release/2.x",
        ])
        .await?;
    let pushed_child = find_pr(&harness, "pushed-child").await?;

    contributor
        .git_ok(
            ["checkout", "-b", "sent-child", &parent_one],
            "create sent child",
        )
        .await?;
    commit_file(
        &contributor,
        "sent-child-one.md",
        "child one\n",
        "sent child one",
    )
    .await?;
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
    let sent_child = find_pr(&harness, "sent-child").await?;

    for child in [&pushed_child, &sent_child] {
        assert_eq!(
            tag_value(child, "merge-base").as_deref(),
            Some(parent_one.as_str())
        );
        assert_eq!(tag_value(child, "b").as_deref(), Some("release/2.x"));
    }

    contributor
        .git_ok(["checkout", "pr/parent"], "advance parent PR")
        .await?;
    commit_file(&contributor, "parent-two.md", "two\n", "parent two").await?;
    let parent_two = contributor.rev_parse("HEAD").await?;
    contributor.nostr_push(["origin", "pr/parent"]).await?;

    contributor
        .git_ok(
            ["checkout", "-B", "pr/pushed-child", &parent_one],
            "rewrite pushed child on stale parent",
        )
        .await?;
    commit_file(
        &contributor,
        "pushed-child-stale.md",
        "stale\n",
        "stale pushed child",
    )
    .await?;
    contributor
        .nostr_push_expecting_failure(["--force", "origin", "pr/pushed-child"])
        .await?;

    contributor
        .git_ok(
            ["checkout", "-B", "sent-child", &parent_one],
            "rewrite sent child on stale parent",
        )
        .await?;
    commit_file(
        &contributor,
        "sent-child-stale.md",
        "stale\n",
        "stale sent child",
    )
    .await?;
    ngit_fails(
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

    contributor
        .git_ok(
            ["checkout", "-B", "pr/pushed-child", &parent_two],
            "rebase pushed child onto latest parent",
        )
        .await?;
    commit_file(
        &contributor,
        "pushed-child-two.md",
        "child two\n",
        "pushed child two",
    )
    .await?;
    let pushed_update_tip = contributor.rev_parse("HEAD").await?;
    contributor
        .nostr_push(["--force", "origin", "pr/pushed-child"])
        .await?;

    contributor
        .git_ok(
            ["checkout", "-B", "sent-child", &parent_two],
            "rebase sent child onto latest parent",
        )
        .await?;
    commit_file(
        &contributor,
        "sent-child-two.md",
        "child two\n",
        "sent child two",
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
            &sent_child.id.to_hex(),
        ],
    )
    .await?;

    for update in [
        find_pr_update_at(&harness, &pushed_update_tip).await?,
        find_pr_update_at(&harness, &sent_update_tip).await?,
    ] {
        assert_eq!(
            tag_value(&update, "merge-base").as_deref(),
            Some(parent_two.as_str())
        );
        assert!(tag_value(&update, "b").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn children_remain_mergeable_after_parent_is_merged_with_no_ff_commit() -> Result<()> {
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
            identifier: Some("merged-stack-base-test".into()),
            ..Default::default()
        })
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "merged stack contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(["checkout", "-b", "pr/parent"], "create parent PR")
        .await?;
    commit_file(&contributor, "parent.md", "parent\n", "parent change").await?;
    let parent_tip = contributor.rev_parse("HEAD").await?;
    contributor
        .nostr_push(["-u", "origin", "pr/parent"])
        .await?;
    let parent = find_pr(&harness, "parent").await?;

    contributor
        .git_ok(
            ["checkout", "-b", "pr/pushed-child", &parent_tip],
            "create pushed child",
        )
        .await?;
    commit_file(
        &contributor,
        "pushed-child.md",
        "pushed child\n",
        "pushed child change",
    )
    .await?;
    contributor
        .nostr_push(["-u", "origin", "pr/pushed-child"])
        .await?;
    let pushed_child = find_pr(&harness, "pushed-child").await?;

    contributor
        .git_ok(
            ["checkout", "-b", "sent-child", &parent_tip],
            "create sent child",
        )
        .await?;
    commit_file(
        &contributor,
        "sent-child.md",
        "sent child\n",
        "sent child change",
    )
    .await?;
    ngit_ok(&contributor, &["send", "--defaults", "--force-pr"]).await?;
    let sent_child = find_pr(&harness, "sent-child").await?;

    for child in [&pushed_child, &sent_child] {
        assert_eq!(
            tag_value(child, "merge-base").as_deref(),
            Some(parent_tip.as_str())
        );
    }

    ngit_ok(&publisher, &["merge", &parent.id.to_hex()]).await?;
    let parent_merge = publisher.rev_parse("main").await?;
    assert_ne!(parent_merge, parent_tip);
    assert_eq!(publisher.rev_parse("main^1").await?, published.initial_oid);
    assert_eq!(publisher.rev_parse("main^2").await?, parent_tip);
    publisher.nostr_push(["origin", "main"]).await?;

    let applied = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitStatusApplied))
        .await?;
    assert!(
        applied
            .iter()
            .any(|event| event_root_e_tag(event) == Some(parent.id))
    );

    contributor
        .git_ok(["fetch", "origin"], "fetch merged parent")
        .await?;
    assert_eq!(contributor.rev_parse("origin/main").await?, parent_merge);

    contributor
        .git_ok(["checkout", "pr/pushed-child"], "update pushed child")
        .await?;
    commit_file(
        &contributor,
        "pushed-child-update.md",
        "pushed child update\n",
        "update pushed child",
    )
    .await?;
    let pushed_update_tip = contributor.rev_parse("HEAD").await?;
    contributor
        .nostr_push(["origin", "pr/pushed-child"])
        .await?;

    contributor
        .git_ok(["checkout", "sent-child"], "update sent child")
        .await?;
    commit_file(
        &contributor,
        "sent-child-update.md",
        "sent child update\n",
        "update sent child",
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
            &sent_child.id.to_hex(),
        ],
    )
    .await?;

    for update in [
        find_pr_update_at(&harness, &pushed_update_tip).await?,
        find_pr_update_at(&harness, &sent_update_tip).await?,
    ] {
        assert_eq!(
            tag_value(&update, "merge-base").as_deref(),
            Some(parent_tip.as_str())
        );
    }

    ngit_ok(&publisher, &["merge", &pushed_child.id.to_hex()]).await?;
    assert_eq!(publisher.rev_parse("main^1").await?, parent_merge);
    assert_eq!(publisher.rev_parse("main^2").await?, pushed_update_tip);
    Ok(())
}

#[tokio::test]
async fn unrelated_stack_candidates_fail_closed_through_send_and_git_push() -> Result<()> {
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
            identifier: Some("ambiguous-pr-base-test".into()),
            ..Default::default()
        })
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "ambiguous stack contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(["checkout", "-b", "pr/left"], "create left parent")
        .await?;
    commit_file(&contributor, "left.md", "left\n", "left parent").await?;
    contributor.nostr_push(["-u", "origin", "pr/left"]).await?;

    contributor
        .git_ok(["checkout", "main"], "return to main")
        .await?;
    contributor
        .git_ok(["checkout", "-b", "pr/right"], "create right parent")
        .await?;
    commit_file(&contributor, "right.md", "right\n", "right parent").await?;
    contributor.nostr_push(["-u", "origin", "pr/right"]).await?;

    contributor
        .git_ok(["checkout", "pr/left"], "return to left parent")
        .await?;
    contributor
        .git_ok(
            ["checkout", "-b", "pr/ambiguous-push"],
            "create ambiguous pushed child",
        )
        .await?;
    contributor
        .git_ok(
            ["merge", "--no-ff", "pr/right", "-m", "combine parents"],
            "combine unrelated parent tips",
        )
        .await?;
    let combined_tip = contributor.rev_parse("HEAD").await?;
    contributor
        .nostr_push_expecting_failure(["-u", "origin", "pr/ambiguous-push"])
        .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "ambiguous-send", &combined_tip],
            "create ambiguous sent child",
        )
        .await?;
    ngit_fails(&contributor, &["send", "--defaults", "--force-pr"]).await?;

    let proposals = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST))
        .await?;
    assert!(proposals.iter().all(|event| {
        !["ambiguous-push", "ambiguous-send"]
            .contains(&event_branch_name_tag(event).unwrap_or_default().as_str())
    }));
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
    let (publisher, published) = harness
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
    ngit_ok(&publisher, &["pr", "checkout", &patch_root.id.to_hex()]).await?;
    ngit_ok(
        &publisher,
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

    commit_file(
        &contributor,
        "upgrade-parent-update.md",
        "upgrade parent update\n",
        "update maintainer-upgraded parent",
    )
    .await?;
    let latest_upgrade_tip = contributor.rev_parse("HEAD").await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &upgrade_root.id.to_hex(),
        ],
    )
    .await?;
    assert_ne!(
        tag_value(
            &find_pr_update_at(&harness, &latest_upgrade_tip).await?,
            "merge-base"
        )
        .as_deref(),
        Some(upgrade_tip.as_str()),
        "the upgraded PR must not infer its own previous tip as its parent"
    );

    contributor
        .git_ok(
            ["checkout", "-b", "pr/upgrade-push", &latest_upgrade_tip],
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
            ["checkout", "-b", "upgrade-send", &latest_upgrade_tip],
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

    contributor
        .git_ok(
            ["checkout", "-b", "pr/upgrade-auto", &latest_upgrade_tip],
            "create automatically based child from maintainer upgrade",
        )
        .await?;
    commit_file(
        &contributor,
        "upgrade-auto.md",
        "upgrade auto\n",
        "automatic upgrade child",
    )
    .await?;
    contributor
        .nostr_push(["-u", "origin", "pr/upgrade-auto"])
        .await?;

    for proposal in [
        find_pr(&harness, "upgrade-push").await?,
        find_pr(&harness, "upgrade-send").await?,
        find_pr(&harness, "upgrade-auto").await?,
    ] {
        assert_eq!(
            tag_value(&proposal, "merge-base").as_deref(),
            Some(latest_upgrade_tip.as_str())
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
