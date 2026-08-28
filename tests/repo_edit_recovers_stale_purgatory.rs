//! Regression for issue `20187ae9`: repeated repository-state events can be
//! stranded in a GRASP server's purgatory after their local branch names are
//! deleted.
//!
//! The repair crosses the ngit/ngit-grasp boundary. `ngit repo edit` must
//! re-sign the cached snapshot, source deleted branches directly from their
//! object IDs, and establish the candidate through the shared state
//! transaction. Once ngit-grasp promotes that candidate it prunes superseded,
//! unreconstructable same-author states. The repository must then support
//! ordinary updates, deletions, and a fresh clone without manual branch
//! recreation.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use nostr::event::FinalizeEvent;
use nostr_sdk::prelude::*;
use test_harness::{CloneLogin, Harness, PublishRepoOpts};

const IDENTIFIER: &str = "init-stale-purgatory-recovery";
const DISPLAY_NAME: &str = "Init stale purgatory recovery";
const STATE_KIND: Kind = Kind::Custom(30618);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_edit_recovers_deleted_state_refs_and_leaves_repo_usable() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let default_relay_url = harness.relay("default").url().to_string();
    let grasp = harness.grasp("repo");
    let grasp_relay_url = grasp.relay_url();
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some(DISPLAY_NAME.to_string()),
            identifier: Some(IDENTIFIER.to_string()),
            extra_repo_relays: vec![default_relay_url.clone()],
            ..Default::default()
        })
        .await?;

    let initial_state = latest_state(grasp, published.maintainer_keys.public_key())
        .await?
        .context("initial ngit init/push did not promote a repository state")?;

    publisher
        .git_ok(["checkout", "-b", "topic-a"], "create topic-a")
        .await?;
    commit_file(&publisher, "topic-a.txt", "topic a\n", "topic a").await?;
    let topic_a_oid = ref_oid(&publisher, "refs/heads/topic-a")?;

    publisher
        .git_ok(["checkout", "main"], "return to main")
        .await?;
    publisher
        .git_ok(["checkout", "-b", "topic-b"], "create topic-b")
        .await?;
    commit_file(&publisher, "topic-b.txt", "topic b\n", "topic b").await?;
    let topic_b_oid = ref_oid(&publisher, "refs/heads/topic-b")?;

    publisher
        .git_ok(["checkout", "main"], "return to main")
        .await?;
    commit_file(&publisher, "main.txt", "main advanced\n", "advance main").await?;
    let advanced_main_oid = ref_oid(&publisher, "refs/heads/main")?;

    // Reproduce the report: several successively newer states reach the
    // relay but their Git snapshot never reaches the GRASP git server. The
    // newest state declares branch names that are about to disappear locally.
    let created_at = initial_state.created_at.as_secs();
    let stale_states = [
        state_event(
            &published.maintainer_keys,
            created_at + 1,
            &published.initial_oid,
            &[("topic-a", &topic_a_oid)],
        )?,
        state_event(
            &published.maintainer_keys,
            created_at + 2,
            &published.initial_oid,
            &[("topic-b", &topic_b_oid)],
        )?,
        state_event(
            &published.maintainer_keys,
            created_at + 3,
            &published.initial_oid,
            &[("topic-a", &topic_a_oid), ("topic-b", &topic_b_oid)],
        )?,
    ];
    for event in &stale_states {
        publish_event(event, &[&grasp_relay_url, &default_relay_url]).await?;
    }

    publisher
        .git_ok(
            ["branch", "-D", "topic-a", "topic-b"],
            "delete local topic branches",
        )
        .await?;
    assert_local_topics_absent(&publisher)?;

    // This is the operation that required manual branch recreation in the
    // original report. The repository edit recovers both refs from their raw
    // OIDs and does not recreate the local branch names.
    let edit = publisher
        .ngit(["repo", "edit", "--name", DISPLAY_NAME, "--defaults"])
        .output()
        .await
        .context("spawn ngit repo edit")?;
    require_success("ngit repo edit", &edit)?;
    assert_local_topics_absent(&publisher)?;

    let bare_repo = grasp
        .git_data_path()
        .join(&published.maintainer_npub)
        .join(format!("{IDENTIFIER}.git"));
    assert_eq!(
        bare_ref(&bare_repo, "refs/heads/main").await?,
        published.initial_oid
    );
    assert_eq!(
        bare_ref(&bare_repo, "refs/heads/topic-a").await?,
        topic_a_oid
    );
    assert_eq!(
        bare_ref(&bare_repo, "refs/heads/topic-b").await?,
        topic_b_oid
    );

    let repaired = latest_state(grasp, published.maintainer_keys.public_key())
        .await?
        .context("repaired state was not promoted from GRASP purgatory")?;
    assert_ne!(repaired.id, stale_states[2].id);
    assert_eq!(
        state_refs(&repaired).get("refs/heads/topic-a"),
        Some(&topic_a_oid)
    );
    assert_eq!(
        state_refs(&repaired).get("refs/heads/topic-b"),
        Some(&topic_b_oid)
    );

    // Prove this was recovery rather than a one-shot lucky push: advance the
    // repaired snapshot normally, then delete the recovered remote branches.
    publisher
        .nostr_push(["origin", "main"])
        .await
        .context("push advanced main after repair")?;
    assert_eq!(
        bare_ref(&bare_repo, "refs/heads/main").await?,
        advanced_main_oid
    );
    publisher
        .nostr_push(["origin", "--delete", "topic-a", "topic-b"])
        .await
        .context("delete recovered topic branches")?;
    assert!(
        bare_ref_optional(&bare_repo, "refs/heads/topic-a")
            .await?
            .is_none()
    );
    assert!(
        bare_ref_optional(&bare_repo, "refs/heads/topic-b")
            .await?
            .is_none()
    );

    let final_state = latest_state(grasp, published.maintainer_keys.public_key())
        .await?
        .context("final main-only state was not promoted")?;
    let final_refs = state_refs(&final_state);
    assert_eq!(final_refs.get("refs/heads/main"), Some(&advanced_main_oid));
    assert!(!final_refs.contains_key("refs/heads/topic-a"));
    assert!(!final_refs.contains_key("refs/heads/topic-b"));

    let clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await
        .context("fresh nostr clone after repair")?;
    assert_eq!(ref_oid(&clone, "refs/heads/main")?, advanced_main_oid);
    assert!(
        !clone
            .snapshot()?
            .refs
            .contains_key("refs/remotes/origin/topic-a")
    );
    assert!(
        !clone
            .snapshot()?
            .refs
            .contains_key("refs/remotes/origin/topic-b")
    );

    Ok(())
}

fn state_event(
    keys: &Keys,
    created_at: u64,
    main_oid: &str,
    topics: &[(&str, &String)],
) -> Result<Event> {
    let mut tags = vec![
        Tag::identifier(IDENTIFIER),
        Tag::parse(["HEAD", "ref: refs/heads/main"])?,
        Tag::parse(["refs/heads/main", main_oid])?,
    ];
    for (name, oid) in topics {
        tags.push(Tag::parse([format!("refs/heads/{name}"), oid.to_string()])?);
    }
    EventBuilder::new(STATE_KIND, "")
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(created_at))
        .finalize(keys)
        .context("sign stale repository-state event")
}

async fn publish_event(event: &Event, relay_urls: &[&str]) -> Result<()> {
    let client = Client::default();
    for relay_url in relay_urls {
        client.add_relay(*relay_url).await?;
    }
    client.connect().await;
    let output = client
        .send_event(event)
        .to(relay_urls.iter().copied())
        .await?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "relay rejected stale state {}: {:?}",
            event.id,
            output.failed
        );
    }
    Ok(())
}

async fn latest_state(
    grasp: &test_harness::GraspServer,
    author: PublicKey,
) -> Result<Option<Event>> {
    let events = grasp
        .events(Filter::new().author(author).kind(STATE_KIND))
        .await?;
    Ok(events
        .into_iter()
        .filter(|event| {
            event.tags.iter().any(|tag| {
                let values = tag.as_slice();
                values.first().map(String::as_str) == Some("d")
                    && values.get(1).map(String::as_str) == Some(IDENTIFIER)
            })
        })
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        }))
}

fn state_refs(event: &Event) -> BTreeMap<String, String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let values = tag.as_slice();
            let name = values.first()?;
            name.starts_with("refs/")
                .then(|| (name.clone(), values.get(1).cloned().unwrap_or_default()))
        })
        .collect()
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

fn ref_oid(repo: &test_harness::Repo, ref_name: &str) -> Result<String> {
    repo.snapshot()?
        .refs
        .get(ref_name)
        .cloned()
        .with_context(|| format!("missing {ref_name}"))
}

fn assert_local_topics_absent(repo: &test_harness::Repo) -> Result<()> {
    let refs = repo.snapshot()?.refs;
    assert!(!refs.contains_key("refs/heads/topic-a"));
    assert!(!refs.contains_key("refs/heads/topic-b"));
    Ok(())
}

async fn bare_ref(repo: &std::path::Path, ref_name: &str) -> Result<String> {
    bare_ref_optional(repo, ref_name)
        .await?
        .with_context(|| format!("missing {ref_name} in {}", repo.display()))
}

async fn bare_ref_optional(repo: &std::path::Path, ref_name: &str) -> Result<Option<String>> {
    let output = tokio::process::Command::new("git")
        .args(["for-each-ref", ref_name, "--format=%(objectname)"])
        .current_dir(repo)
        .output()
        .await?;
    require_success("inspect GRASP bare ref", &output)?;
    let oid = String::from_utf8(output.stdout)?.trim().to_string();
    Ok((!oid.is_empty()).then_some(oid))
}

fn require_success(label: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}
