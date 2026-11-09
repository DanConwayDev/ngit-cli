use anyhow::{Context, Result};
use nostr::prelude::{EventBuilder, Keys, Kind, Tag, Timestamp, event::FinalizeEvent};
use test_harness::{Harness, PublishRepoOpts};

#[tokio::test]
async fn service_view_is_repository_scoped_and_distinguishes_request_from_acting() -> Result<()> {
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
    let root = format!(
        "30617:{}:{}",
        published.maintainer_keys.public_key(),
        published.identifier
    );
    let c = Keys::generate();
    let other = Keys::generate();
    let now = Timestamp::now();
    let make = |keys: &Keys, kind: u16, tags: Vec<Tag>| {
        EventBuilder::new(Kind::Custom(kind), "")
            .custom_created_at(now)
            .tags(tags)
            .finalize(keys)
    };
    let advertisement = |keys: &Keys| {
        make(
            keys,
            19843,
            vec![
                Tag::custom("M", ["maintainer-request"]),
                Tag::custom("X", ["request-required"]),
                Tag::custom("expiration", [(now.as_secs() + 1200).to_string()]),
            ],
        )
    };
    let readiness = make(
        &c,
        19844,
        vec![
            Tag::custom("a", [root.clone()]),
            Tag::custom("expiration", [(now.as_secs() + 1200).to_string()]),
        ],
    )?;
    harness
        .publish_ci_events(
            &relay,
            &[advertisement(&c)?, advertisement(&other)?, readiness],
        )
        .await?;
    for offline in [false, true] {
        let mut args = vec!["ci", "service", "status", "--json"];
        if offline {
            args.push("--offline");
        }
        let output = repo.ngit(args).output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let rows = json["services"].as_array().context("services")?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["advertised_ready"], true);
        assert_eq!(rows[0]["service"], "no_status_observed");
        assert_eq!(rows[0]["request"], "none_observed");
    }
    let request = make(
        &published.maintainer_keys,
        9843,
        vec![
            Tag::custom("a", [root.clone()]),
            Tag::custom("p", [c.public_key().to_hex()]),
        ],
    )?;
    let acting = make(
        &c,
        39844,
        vec![
            Tag::custom("d", [root.clone()]),
            Tag::custom("a", [root]),
            Tag::custom("s", ["acting"]),
            Tag::custom("expiration", [(now.as_secs() + 1200).to_string()]),
        ],
    )?;
    harness
        .publish_ci_events(&relay, &[request, acting])
        .await?;
    let output = repo
        .ngit(["ci", "service", "status", "--json"])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["services"][0]["service"], "acting");
    assert_eq!(json["services"][0]["request"], "maintainer_requested");
    let inspection = repo.ngit(["ci", "inspect", "--json"]).output().await?;
    assert!(inspection.status.success());
    let inspected: serde_json::Value = serde_json::from_slice(&inspection.stdout)?;
    assert!(
        inspected["ci"]["runs"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(
        inspected.get("events").is_none(),
        "default inspection must not list service advertisements"
    );
    // Workflow activity discovers a coordinator even without service claims.
    // A maintainer request without activity remains visible alongside it.
    let idle = Keys::generate();
    let root = test_harness::repo_coordinate(&published);
    let idle_request = make(
        &published.maintainer_keys,
        9843,
        vec![
            Tag::custom("a", [root]),
            Tag::custom("p", [idle.public_key().to_hex()]),
        ],
    )?;
    let mut spec = test_harness::CiRunSpec::new(
        &published,
        "observed",
        "1".repeat(40),
        test_harness::CiTrigger::push("refs/heads/main"),
        now.as_secs(),
    );
    let trigger = test_harness::build_manual_trigger(
        &published.maintainer_keys,
        &other.public_key(),
        &spec,
        spec.started_at - 1,
    )?;
    spec.provenance = Some(test_harness::CiProvenance::manual_trigger(&trigger));
    let run = test_harness::build_ci_run(&other, &spec)?;
    let mut events = run.all();
    events.push(trigger);
    events.push(idle_request);
    harness.publish_ci_events(&relay, &events).await?;
    for offline in [false, true] {
        let mut args = vec!["ci", "service", "status", "--json"];
        if offline {
            args.push("--offline");
        }
        let output = repo.ngit(args).output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let rows = json["services"].as_array().context("services")?;
        assert_eq!(rows.len(), 3);
        let active = rows
            .iter()
            .find(|r| r["coordinator"] == other.public_key().to_hex())
            .context("active coordinator")?;
        assert_eq!(active["activity"]["runs_observed"], 1);
        assert_eq!(active["activity"]["maintainer_directed_runs"], 1);
        assert_eq!(
            active["activity"]["latest"]["classification"],
            "maintainer-directed"
        );
        assert_eq!(active["activity"]["latest"]["status"], "success");
        assert_eq!(active["request"], "none_observed");
        let idle = rows
            .iter()
            .find(|r| r["coordinator"] == idle.public_key().to_hex())
            .context("idle coordinator")?;
        assert_eq!(idle["request"], "maintainer_requested");
        assert_eq!(idle["activity"]["runs_observed"], 0);
        assert!(idle["activity"]["latest"].is_null());
    }
    Ok(())
}
