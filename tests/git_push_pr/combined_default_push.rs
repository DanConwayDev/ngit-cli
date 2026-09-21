//! Publishing the default branch in the same push makes it an explicit PR base.
use anyhow::Result;
use nostr_sdk::prelude::*;
use rstest::rstest;
use test_harness::{CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, tag_value};

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test(flavor = "multi_thread")]
async fn default_and_proposal_publish_together_without_force(
    #[case] proposal_first: bool,
) -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (publisher, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    std::fs::write(publisher.dir().join("accepted.txt"), "accepted change")?;
    publisher
        .git_ok(["add", "accepted.txt"], "stage default advance")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main", "--no-gpg-sign"],
            "advance local default",
        )
        .await?;
    let new_default = publisher.rev_parse("HEAD").await?;
    publisher
        .git_ok(["checkout", "-b", "pr/feature"], "start proposal")
        .await?;
    std::fs::write(publisher.dir().join("feature.txt"), "proposed change")?;
    publisher
        .git_ok(["add", "feature.txt"], "stage feature")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "feature", "--no-gpg-sign"],
            "commit feature",
        )
        .await?;
    let proposal_tip = publisher.rev_parse("HEAD").await?;
    assert_ne!(new_default, published.initial_oid);

    let refs = if proposal_first {
        ["pr/feature", "main"]
    } else {
        ["main", "pr/feature"]
    };
    publisher.nostr_push(["origin", refs[0], refs[1]]).await?;

    let proposals = harness
        .grasp("repo")
        .events(Filter::new().kind(KIND_PULL_REQUEST))
        .await?;
    assert_eq!(proposals.len(), 1);
    assert_eq!(
        tag_value(&proposals[0], "merge-base").as_deref(),
        Some(new_default.as_str())
    );
    assert_eq!(
        tag_value(&proposals[0], "c").as_deref(),
        Some(proposal_tip.as_str())
    );
    let state = ngit::repo_state::RepoState::try_from(
        harness
            .grasp("repo")
            .events(Filter::new().kind(Kind::from(30618u16)))
            .await?,
    )?;
    assert_eq!(state.state.get("refs/heads/main"), Some(&new_default));
    assert_eq!(
        publisher.rev_parse("refs/remotes/origin/main").await?,
        new_default
    );
    assert_eq!(
        publisher
            .rev_parse("refs/remotes/origin/pr/feature")
            .await?,
        proposal_tip
    );
    let clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    assert_eq!(clone.rev_parse("main").await?, new_default);
    Ok(())
}
