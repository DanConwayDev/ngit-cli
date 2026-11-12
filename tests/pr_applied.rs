//! Explicit status repair must leave Git state untouched and be retryable.
use anyhow::{Context, Result, ensure};
use nostr::prelude::{Filter, Keys, Kind};
use test_harness::{Harness, PublishPrOpts, PublishRepoOpts};

#[tokio::test]
async fn applied_status_is_authorized_idempotent_and_does_not_mutate_git() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (repo, published) = harness.publish_repo(PublishRepoOpts::default()).await?;
    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: None,
                commits: vec![],
                title: "status repair".into(),
                description: "equivalent change".into(),
                in_reply_to: vec![],
            },
        )
        .await?;
    let id = pr.event_id.to_hex();
    let stranger = harness
        .clone_published_repo_as(&published, &Keys::generate())
        .await?;
    let refusal = stranger.ngit(["pr", "applied", &id]).output().await?;
    ensure!(!refusal.status.success());
    ensure!(
        String::from_utf8_lossy(&refusal.stderr)
            .contains("only the PR author or a repository member")
    );
    // Model equivalent work landing through another commit, without PR ancestry.
    repo.git_ok(
        [
            "commit",
            "--allow-empty",
            "-m",
            "equivalent change",
            "--no-gpg-sign",
        ],
        "land alternative",
    )
    .await?;
    repo.nostr_push(["origin", "main"]).await?;
    std::fs::write(repo.dir().join("local.txt"), "preserve my work")?;
    repo.git_ok(["add", "local.txt"], "stage work").await?;
    std::fs::write(repo.dir().join("local.txt"), "also preserve unstaged work")?;
    let head = repo.rev_parse("HEAD").await?;
    let refs = repo.git(["show-ref"]).output().await?.stdout;
    let index = repo.git(["diff", "--cached"]).output().await?.stdout;
    let work = repo.git(["diff"]).output().await?.stdout;
    for expected in ["applied", "unchanged"] {
        let result = repo
            .ngit([
                "pr",
                "applied",
                &id,
                "--reason",
                "incorporated by equivalent change",
                "--json",
            ])
            .output()
            .await?;
        ensure!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&result.stdout).context("status JSON")?;
        assert_eq!(json["action"], expected);
        assert_eq!(repo.rev_parse("HEAD").await?, head);
        assert_eq!(repo.git(["show-ref"]).output().await?.stdout, refs);
        assert_eq!(repo.git(["diff", "--cached"]).output().await?.stdout, index);
        assert_eq!(repo.git(["diff"]).output().await?.stdout, work);
    }
    let statuses = harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitStatusApplied)
                .event(pr.event_id),
        )
        .await?;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].pubkey, published.maintainer_keys.public_key());
    assert_eq!(statuses[0].content, "incorporated by equivalent change");
    Ok(())
}
