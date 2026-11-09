//! Watch the signed event stream, including intermediate snapshots which a
//! latest-state cache cannot reconstruct. Test deadlines do not limit the CLI.
use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result};
use nostr::prelude::{Event, EventBuilder, Keys, Kind, Tag, Timestamp, event::FinalizeEvent};
use serde_json::Value;
use test_harness::{Harness, PublishRepoOpts, repo_coordinate};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};

async fn next(lines: &mut Lines<BufReader<tokio::process::ChildStdout>>) -> Result<Value> {
    let line = tokio::time::timeout(Duration::from_secs(45), lines.next_line())
        .await??
        .context("watch ended")?;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test]
async fn watch_emits_intermediate_receipts_once_and_stays_open_after_completion() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("ci")
    .with_relay("duplicate")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("ci").url().to_string();
    let duplicate = harness.relay("duplicate").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            extra_repo_relays: vec![relay.clone(), duplicate.clone()],
            ..Default::default()
        })
        .await?;
    let commit = repo.rev_parse("HEAD").await?;
    let keys = Keys::generate();
    let base = Timestamp::now().as_secs() - 100;
    let event = |kind: u16, n: u64, extra: Vec<Tag>| -> Result<Event> {
        let mut tags = vec![
            Tag::custom("a", [repo_coordinate(&published)]),
            Tag::custom("c", [&commit]),
            Tag::custom("w", [".ngit/nix/workflows/ci.yaml", &"aa".repeat(32)]),
            Tag::custom("d", ["aa".repeat(32)]),
            Tag::custom("job", ["build"]),
            Tag::custom("expiration", [(base + 1000).to_string()]),
        ];
        tags.extend(extra);
        Ok(EventBuilder::new(Kind::Custom(kind), "")
            .custom_created_at(Timestamp::from(base + n))
            .tags(tags)
            .finalize(&keys)?)
    };
    let seed = event(39842, 0, vec![Tag::custom("status", ["in_progress"])])?;
    harness
        .publish_ci_events(&relay, std::slice::from_ref(&seed))
        .await?;
    let mut child = repo
        .ngit(["ci", "status", "--watch", "--follow", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(child.stdout.take().context("stdout")?).lines();
    assert_eq!(next(&mut lines).await?["event"]["id"], seed.id.to_hex());
    let progress1 = event(
        39846,
        1,
        vec![Tag::custom(
            "step",
            ["compile", "Compile", "0", "1250", "success"],
        )],
    )?;
    let progress2 = event(
        39846,
        2,
        vec![
            Tag::custom("step", ["compile", "Compile", "0", "1250", "success"]),
            Tag::custom("step", ["test", "Test", "1250", "500", "success"]),
            Tag::custom("future", ["", "extension"]),
        ],
    )?;
    for event in [&progress1, &progress2] {
        harness
            .publish_ci_events(&relay, std::slice::from_ref(event))
            .await?;
        harness
            .publish_ci_events(&duplicate, std::slice::from_ref(event))
            .await?;
        let record = next(&mut lines).await?;
        assert_eq!(record["event"]["id"], event.id.to_hex());
        assert!(record["event"].get("sig").is_none());
        assert_eq!(record["signature_verified"], true);
    }
    let job = event(9841, 3, vec![Tag::custom("conclusion", ["success"])])?;
    let workflow = event(9842, 4, vec![Tag::custom("conclusion", ["success"])])?;
    let later = event(39842, 5, vec![Tag::custom("status", ["queued"])])?;
    for event in [&job, &workflow, &later] {
        harness
            .publish_ci_events(&relay, std::slice::from_ref(event))
            .await?;
        assert_eq!(next(&mut lines).await?["event"]["id"], event.id.to_hex());
    }
    #[cfg(unix)]
    {
        let pid = child.id().context("child exited")?;
        // SIGINT is scoped to this child; the harness kills it on drop if cleanup
        // fails.
        unsafe {
            libc::kill(i32::try_from(pid)?, libc::SIGINT);
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(15), child.wait())
                .await??
                .success()
        );
        assert!(
            lines.next_line().await?.is_none(),
            "no trailing JSON document"
        );
    }
    #[cfg(not(unix))]
    child.kill().await?;
    let failed = repo
        .ngit(["ci", "status", "no-such-ci-target", "--watch", "--json"])
        .output()
        .await?;
    assert!(!failed.status.success());
    assert!(
        failed.stdout.is_empty(),
        "startup errors must not corrupt the event stream"
    );
    Ok(())
}

#[tokio::test]
async fn completed_watch_exits_after_grace_without_a_trailing_json_document() -> Result<()> {
    use test_harness::{CiProgress, CiRunSpec, CiTrigger, build_ci_run, workflow_hash};
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("ci")
    .with_grasp_server("repo")
    .build()
    .await?;
    let relay = harness.relay("ci").url().to_string();
    let (repo, published) = harness
        .publish_repo(PublishRepoOpts {
            extra_repo_relays: vec![relay.clone()],
            ..Default::default()
        })
        .await?;
    let spec = CiRunSpec::new(
        &published,
        "finished",
        repo.rev_parse("HEAD").await?,
        CiTrigger::push("refs/heads/main"),
        Timestamp::now().as_secs(),
    );
    let coordinator = Keys::generate();
    let run = build_ci_run(&coordinator, &spec)?;
    harness.publish_ci_events(&relay, &run.all()).await?;
    let mut child = repo
        .ngit(["ci", "status", "--watch", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(child.stdout.take().context("stdout")?).lines();
    assert_eq!(next(&mut lines).await?["event"]["kind"], 9842);
    let now = Timestamp::now().as_secs();
    let mut dependent = CiRunSpec::new(
        &published,
        "dependent",
        repo.rev_parse("HEAD").await?,
        CiTrigger::push("refs/heads/main"),
        now,
    )
    .workflow("deploy.yml", workflow_hash(""))
    .conclusion(None)
    .progress(CiProgress {
        status: "queued".into(),
        created_at: now,
        expiration: now + 120,
        conclusion: None,
    });
    harness
        .publish_ci_events(&relay, &build_ci_run(&coordinator, &dependent)?.all())
        .await?;
    assert_eq!(next(&mut lines).await?["event"]["kind"], 39842);
    // Passage of the completion grace is the behavior under test. Waiting on
    // process exit verifies queued work keeps observation alive beyond it.
    assert!(
        tokio::time::timeout(Duration::from_secs(11), child.wait())
            .await
            .is_err()
    );
    dependent.progress = None;
    dependent.conclusion = Some("success".into());
    dependent.created_at = Timestamp::now().as_secs();
    harness
        .publish_ci_events(&relay, &build_ci_run(&coordinator, &dependent)?.all())
        .await?;
    assert_eq!(next(&mut lines).await?["event"]["kind"], 9842);
    assert!(
        tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await??
            .success()
    );
    assert!(
        lines.next_line().await?.is_none(),
        "no trailing aggregate document"
    );
    // A fresh watcher of an already-completed commit also finishes from its
    // initial snapshot, without needing another relay event to start the grace.
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        repo.ngit(["ci", "status", "--watch", "--json"]).output(),
    )
    .await??;
    assert!(output.status.success());
    for line in String::from_utf8(output.stdout)?.lines() {
        let record: Value = serde_json::from_str(line)?;
        assert!(record.get("event").is_some(), "only event records");
    }
    Ok(())
}
