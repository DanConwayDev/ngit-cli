//! Real LMDB failures must become a fresh cache and a complete online refresh.

use anyhow::{Context, Result};
use heed::{EnvFlags, EnvOpenOptions, types::Bytes};
use nostr::prelude::*;
use nostr_database::NostrDatabase;
use nostr_lmdb::NostrLmdb;
use test_harness::{Harness, PublishRepoOpts, Repo};

async fn json(repo: &Repo, args: &[&str]) -> Result<serde_json::Value> {
    let output = repo.ngit(args).output().await?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn corrupt_announcement(path: &std::path::Path) -> Result<()> {
    let db = NostrLmdb::open(path).await?;
    let event = db
        .query(Filter::new().kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .next()
        .context("missing cached announcement")?;
    // Leave the index records intact, reproducing the exact backend error in
    // ef38b7f9. Use a transaction so no mapped LMDB pages are edited externally.
    let env = unsafe {
        EnvOpenOptions::new()
            .flags(EnvFlags::NO_TLS)
            .max_dbs(12)
            .max_readers(126)
            .map_size(if usize::BITS == 64 {
                32 * 1024 * 1024 * 1024
            } else {
                0xFFFFF000
            })
            .open(path)?
    };
    let mut txn = env.write_txn()?;
    let records = env
        .open_database::<Bytes, Bytes>(&txn, None)?
        .context("missing event database")?;
    records.delete(&mut txn, event.id.as_bytes())?;
    txn.commit()?;
    assert_eq!(
        db.query(Filter::new().author(event.pubkey).kind(event.kind))
            .await
            .unwrap_err()
            .to_string(),
        "Not found"
    );
    Ok(())
}

async fn exercise_recovery(local: bool, global: bool, concurrent: bool) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let harness = Harness::builder(
            env!("CARGO_BIN_EXE_ngit"),
            env!("CARGO_BIN_EXE_git-remote-nostr"),
        )
        .with_relay("default")
        .with_grasp_server("repo")
        .build()
        .await?;
        let (repo, _) = harness.publish_repo(PublishRepoOpts::default()).await?;
        let created = json(
            &repo,
            &[
                "issue",
                "create",
                "--subject",
                "recover me",
                "--body",
                "original body",
                "--json",
            ],
        )
        .await?;
        let id = created["id"].as_str().context("missing issue id")?;
        json(
            &repo,
            &[
                "issue",
                "comment",
                id,
                "--body",
                "recover this reply too",
                "--json",
            ],
        )
        .await?;
        let git_config = std::fs::read(repo.dir().join(".git/config"))?;
        for (enabled, filename) in [
            (local, "nostr-cache.lmdb"),
            (global, "test-global-cache.lmdb"),
        ] {
            if enabled {
                corrupt_announcement(&repo.dir().join(".git").join(filename)).await?;
            }
        }
        let outputs =
            futures::future::join_all((0..if concurrent { 3 } else { 1 }).map(|_| async {
                json(&repo, &["issue", "view", id, "--comments", "--json"]).await
            }))
            .await;
        for output in outputs {
            let document = output?;
            assert_eq!(document["subject"], "recover me");
            assert_eq!(document["comments"][0]["body"], "recover this reply too");
        }
        // A separate offline command must see the fully rebuilt cache as well.
        let document = json(
            &repo,
            &["issue", "view", id, "--comments", "--offline", "--json"],
        )
        .await?;
        assert_eq!(document["subject"], "recover me");
        assert_eq!(document["comments"][0]["body"], "recover this reply too");
        assert_eq!(std::fs::read(repo.dir().join(".git/config"))?, git_config);
        for (enabled, filename) in [
            (local, "nostr-cache.lmdb"),
            (global, "test-global-cache.lmdb"),
        ] {
            let parent = repo.dir().join(".git");
            assert_eq!(parent.join(format!("{filename}.current")).exists(), enabled);
            assert!(parent.join(filename).join("data.mdb").exists());
            let replacements = std::fs::read_dir(parent)?
                .filter_map(std::result::Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{filename}.recovered-"))
                })
                .count();
            assert_eq!(replacements, usize::from(enabled));
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn local_cache_is_rebuilt_from_relays() -> Result<()> {
    exercise_recovery(true, false, false).await
}

#[tokio::test]
async fn global_cache_is_rebuilt_from_relays() -> Result<()> {
    exercise_recovery(false, true, false).await
}

#[tokio::test]
async fn concurrent_processes_rebuild_both_caches_once() -> Result<()> {
    exercise_recovery(true, true, true).await
}
