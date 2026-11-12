//! Sending a full PR tip sets canonical tracking without retargeting other
//! work.
use anyhow::{Context, Result, ensure};
use test_harness::{Harness, PublishRepoOpts};

async fn tracking_case(branch: &str, existing: bool, partial: bool, patch: bool) -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (repo, _) = harness.publish_repo(PublishRepoOpts::default()).await?;
    repo.git_ok(["remote", "rename", "origin", "upstream"], "rename remote")
        .await?;
    repo.git_ok(["checkout", "--no-track", "-b", branch], "create branch")
        .await?;
    for i in 0..2 {
        std::fs::write(
            repo.dir().join(format!("change-{i}.txt")),
            format!("change {i}"),
        )?;
        repo.git_ok(["add", "."], "stage change").await?;
        repo.git_ok(
            ["commit", "-m", &format!("change {i}"), "--no-gpg-sign"],
            "commit",
        )
        .await?;
    }
    if existing {
        repo.git_ok(
            ["branch", "--set-upstream-to=upstream/main"],
            "existing upstream",
        )
        .await?;
    }
    let out = repo
        .ngit([
            "--repo",
            "upstream",
            "send",
            if partial { "HEAD~2..HEAD~1" } else { "HEAD~2" },
            if patch { "--force-patch" } else { "--force-pr" },
            "--subject",
            "tracking test",
            "--description",
            "published branch",
            "--defaults",
            "--json",
        ])
        .output()
        .await?;
    ensure!(
        out.status.success(),
        "send: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let merge = repo
        .git(["config", "--get", &format!("branch.{branch}.merge")])
        .output()
        .await?;
    if existing {
        assert_eq!(
            String::from_utf8_lossy(&merge.stdout).trim(),
            "refs/heads/main"
        );
    } else if partial || patch || !branch.starts_with("pr/") {
        ensure!(!merge.status.success(), "unrelated work was given tracking");
    } else {
        ensure!(merge.status.success());
        let canonical = String::from_utf8(merge.stdout)?.trim().to_owned();
        ensure!(canonical.starts_with("refs/heads/pr/"));
        let upstream = repo.rev_parse("@{upstream}").await?;
        assert_eq!(upstream, repo.rev_parse("HEAD").await?);
        let remote = repo
            .git(["config", "--get", &format!("branch.{branch}.remote")])
            .output()
            .await?;
        assert_eq!(String::from_utf8_lossy(&remote.stdout).trim(), "upstream");
        // Rename the local branch, remove tracking, then revise the same PR.
        // Its advertised name remains tied to the original root event.
        let sent: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        let root = sent["id"].as_str().context("PR event id")?;
        repo.git_ok(["branch", "-m", "pr/renamed"], "rename local branch")
            .await?;
        repo.git_ok(["branch", "--unset-upstream"], "clear upstream")
            .await?;
        std::fs::write(repo.dir().join("revision.txt"), "revision")?;
        repo.git_ok(["add", "revision.txt"], "stage revision")
            .await?;
        repo.git_ok(["commit", "-m", "revision", "--no-gpg-sign"], "revision")
            .await?;
        let revision = repo
            .ngit([
                "--repo",
                "upstream",
                "send",
                "HEAD~3",
                "--in-reply-to",
                root,
                "--force-pr",
                "--subject",
                "tracking test",
                "--description",
                "revised",
                "--defaults",
                "--json",
            ])
            .output()
            .await?;
        ensure!(
            revision.status.success(),
            "revision: {}",
            String::from_utf8_lossy(&revision.stderr)
        );
        let merge = repo
            .git(["config", "--get", "branch.pr/renamed.merge"])
            .output()
            .await?;
        assert_eq!(String::from_utf8_lossy(&merge.stdout).trim(), canonical);
        let pull = repo
            .git(["pull", "--ff-only"])
            .output()
            .await
            .context("plain pull")?;
        ensure!(
            pull.status.success(),
            "pull: {}",
            String::from_utf8_lossy(&pull.stderr)
        );
    }
    Ok(())
}

#[tokio::test]
async fn send_tracks_canonical_pr_on_selected_remote_and_pull_works() -> Result<()> {
    tracking_case("pr/local-name", false, false, false).await
}
#[tokio::test]
async fn send_preserves_existing_upstream() -> Result<()> {
    tracking_case("pr/existing", true, false, false).await
}
#[tokio::test]
async fn send_does_not_track_partial_ranges() -> Result<()> {
    tracking_case("pr/partial", false, true, false).await
}
#[tokio::test]
async fn send_does_not_track_patches() -> Result<()> {
    tracking_case("pr/patch", false, false, true).await
}
#[tokio::test]
async fn send_does_not_retarget_ordinary_branches() -> Result<()> {
    tracking_case("feature", false, false, false).await
}
