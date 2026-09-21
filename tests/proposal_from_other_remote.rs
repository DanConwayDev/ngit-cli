//! Contributors can explicitly select accepted history from another publishing
//! remote when the Nostr destination has not caught up yet.
use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::rstest;
use test_harness::{CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, tag_value};

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test(flavor = "multi_thread")]
async fn contributor_selects_other_remote_base(#[case] via_push: bool) -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_vanilla_git_server("github")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let github = harness.vanilla_git_server("github").url();
    publisher
        .git_ok(["remote", "add", "github", github], "add publishing remote")
        .await?;
    std::fs::write(
        publisher.dir().join("upstream.txt"),
        "accepted upstream change",
    )?;
    publisher
        .git_ok(["add", "upstream.txt"], "stage upstream change")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "upstream change", "--no-gpg-sign"],
            "commit upstream change",
        )
        .await?;
    publisher
        .git_ok(["push", "github", "main"], "publish only to GitHub fixture")
        .await?;
    let accepted_base = publisher.rev_parse("HEAD").await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(
            ["remote", "rename", "origin", "upstream"],
            "name Nostr destination",
        )
        .await?;
    contributor
        .git_ok(
            ["remote", "add", "github", github],
            "add upstream publishing remote",
        )
        .await?;
    contributor
        .git_ok(["fetch", "github"], "fetch accepted history")
        .await?;
    contributor
        .git_ok(["reset", "--hard", "github/main"], "update local default")
        .await?;
    contributor
        .git_ok(["checkout", "-b", "pr/feature"], "start proposal")
        .await?;
    for index in 0..3 {
        std::fs::write(
            contributor.dir().join("feature.txt"),
            format!("feature {index}"),
        )?;
        contributor
            .git_ok(["add", "feature.txt"], "stage feature")
            .await?;
        contributor
            .git_ok(
                ["commit", "-m", &format!("feature {index}"), "--no-gpg-sign"],
                "commit feature",
            )
            .await?;
    }
    let tip = contributor.rev_parse("HEAD").await?;
    assert_eq!(contributor.rev_parse("HEAD~3").await?, accepted_base);
    assert_eq!(
        contributor.rev_parse("upstream/main").await?,
        published.initial_oid
    );
    assert_ne!(accepted_base, published.initial_oid);

    if via_push {
        contributor
            .nostr_push(["upstream", "pr/feature", "-o", "base=github/main"])
            .await?;
    } else {
        let output = contributor
            .ngit([
                "--repo",
                "upstream",
                "send",
                "--base",
                "github/main",
                "--defaults",
            ])
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "send failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let keys = Keys::parse(
        &contributor
            .config("nostr.nsec")
            .await?
            .context("contributor login")?,
    )?;
    let events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(keys.public_key())
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    assert_eq!(events.len(), 1);
    assert_eq!(
        tag_value(&events[0], "merge-base").as_deref(),
        Some(accepted_base.as_str())
    );
    assert_eq!(tag_value(&events[0], "c").as_deref(), Some(tip.as_str()));
    assert_eq!(
        contributor.rev_parse("upstream/main").await?,
        published.initial_oid
    );
    Ok(())
}
