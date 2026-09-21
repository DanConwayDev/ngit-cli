//! A contributor's fork can already contain the entire change on its default
//! branch. Only the destination repository decides what is already merged.
use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::rstest;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts, tag_value,
};

#[rstest]
#[case(false, false, false)]
#[case(true, false, false)]
#[case(false, true, false)]
#[case(true, true, false)]
#[case(false, false, true)]
#[case(true, false, true)]
#[tokio::test(flavor = "multi_thread")]
async fn proposal_uses_upstream_not_fork(
    #[case] push_by_oid: bool,
    #[case] maintainer: bool,
    #[case] tracks_destination: bool,
) -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_vanilla_git_server("fork")
    .build()
    .await?;
    let (_, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            if maintainer {
                CloneLogin::AsMaintainer
            } else {
                CloneLogin::AsContributor {
                    display_name: "fork contributor".into(),
                }
            },
        )
        .await?;
    contributor
        .git_ok(
            ["remote", "rename", "origin", "upstream"],
            "rename destination",
        )
        .await?;
    contributor
        .git_ok(
            [
                "remote",
                "add",
                "origin",
                harness.vanilla_git_server("fork").url(),
            ],
            "add contributor fork",
        )
        .await?;
    let keys = Keys::parse(&contributor.config("nostr.nsec").await?.context("login")?)?;

    // Open, fast-forward, then rewrite the proposal. Local main and the fork's
    // main already contain each pushed tip, just as in the reported git log.
    for (revision, kind) in [
        KIND_PULL_REQUEST,
        KIND_PULL_REQUEST_UPDATE,
        KIND_PULL_REQUEST_UPDATE,
    ]
    .into_iter()
    .enumerate()
    {
        contributor
            .git_ok(["checkout", "main"], "checkout fork default")
            .await?;
        if revision == 2 {
            contributor
                .git_ok(
                    ["reset", "--hard", &published.initial_oid],
                    "rewrite proposal",
                )
                .await?;
        }
        std::fs::write(
            contributor.dir().join("feature.txt"),
            format!("revision {revision}"),
        )?;
        contributor
            .git_ok(["add", "feature.txt"], "stage feature")
            .await?;
        contributor
            .git_ok(
                [
                    "commit",
                    "-m",
                    &format!("feature revision {revision}"),
                    "--no-gpg-sign",
                ],
                "commit feature on local main",
            )
            .await?;
        let tip = contributor.rev_parse("HEAD").await?;
        contributor
            .git_ok(["push", "--force", "origin", "main"], "publish fork main")
            .await?;
        if !tracks_destination {
            contributor
                .git_ok(
                    ["branch", "--set-upstream-to=origin/main", "main"],
                    "track fork default",
                )
                .await?;
        }
        contributor
            .git_ok(["branch", "-f", "pr/feature", &tip], "set proposal branch")
            .await?;
        contributor
            .git_ok(["checkout", "pr/feature"], "checkout proposal")
            .await?;
        let source = if push_by_oid {
            &tip
        } else {
            "refs/heads/pr/feature"
        };
        let refspec = format!("{source}:refs/heads/pr/feature");
        let mut args = vec!["upstream", refspec.as_str()];
        if revision == 2 {
            args.insert(0, "--force");
        }
        let output = contributor.nostr_push(args).await?;
        let events = harness
            .grasp("repo")
            .events(Filter::new().author(keys.public_key()).kind(kind))
            .await?;
        let event = events
            .iter()
            .find(|e| tag_value(e, "c").as_deref() == Some(tip.as_str()))
            .with_context(|| {
                format!("proposal event for revision {revision}, tip {tip}, events {events:?}, push: {} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
            })?;
        assert_eq!(
            tag_value(event, "merge-base").as_deref(),
            Some(published.initial_oid.as_str())
        );
        assert_eq!(
            contributor.rev_parse("upstream/main").await?,
            published.initial_oid
        );
        assert_eq!(contributor.rev_parse("origin/main").await?, tip);
    }

    // A branch with no changes relative to the destination remains invalid.
    contributor
        .nostr_push_expecting_failure([
            "upstream",
            &format!("{}:refs/heads/pr/empty", published.initial_oid),
        ])
        .await?;
    assert_eq!(
        harness
            .grasp("repo")
            .events(
                Filter::new()
                    .author(keys.public_key())
                    .kind(KIND_PULL_REQUEST)
            )
            .await?
            .len(),
        1
    );
    Ok(())
}
