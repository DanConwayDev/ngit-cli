//! `ngit pr list`'s CI column: one row per state the glyph table
//! distinguishes, computed from the local cache alone.
//!
//! The glyph itself is not asserted on — the table is human output. Each row
//! is checked through the structured JSON fields that drive it: the state,
//! the rolled-up conclusion and whether the weakest current run met the
//! operationally-associated floor.
//!
//! CI fixture events are signed by [`test_harness::ci`] and published
//! straight to a relay the repository announcement lists, so everything a
//! test asserts on is queryable the moment the publish returns — no sleeps,
//! no polling.

use anyhow::{Context, Result};
use nostr::prelude::{Keys, Timestamp};
use serde_json::Value;
use test_harness::{
    CiProgress, CiProvenance, CiRunSpec, CiTrigger, CloneLogin, Harness, PublishPrOpts,
    PublishRepoOpts, PublishedPr, build_ci_run, build_service_control, repo_coordinate,
    workflow_hash,
};

const WORKFLOW: &str = "name: ci\non: push\n";

/// Run `ngit pr list --json` and parse stdout.
async fn pr_list(repo: &test_harness::Repo) -> Result<(std::process::Output, Value)> {
    let argv = ["pr", "list", "--json"];
    let out = repo
        .ngit(argv)
        .output()
        .await
        .context("failed to spawn `ngit pr list`")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let json = serde_json::from_str(&stdout).with_context(|| {
        format!(
            "`ngit {}` stdout is not valid JSON:\n{stdout}\nstderr: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr),
        )
    })?;
    Ok((out, json))
}

/// The `ci` object of the row whose subject is `subject`.
fn row_ci<'a>(json: &'a Value, subject: &str) -> &'a Value {
    let rows = json
        .as_array()
        .unwrap_or_else(|| panic!("`ngit pr list --json` is not an array: {json}"));
    &rows
        .iter()
        .find(|row| row["subject"] == subject)
        .unwrap_or_else(|| panic!("no row with subject {subject:?} in {json}"))["ci"]
}

#[tokio::test]
async fn every_ci_state_gets_its_own_row() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_relay("ci")
    .with_grasp_server("repo")
    .build()
    .await?;

    let ci_relay = harness.relay("ci").url().to_string();
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("list-ci".to_string()),
            initial_file: Some(("ci.yml".to_string(), WORKFLOW.to_string())),
            extra_repo_relays: vec![ci_relay.clone()],
            ..Default::default()
        })
        .await?;

    let now = Timestamp::now().as_secs();
    let coordinator = Keys::generate();
    let stranger = Keys::generate();

    // A confirmed maintainer's standing request, signed before every run
    // below started: it is what separates `✓` from `✓?`.
    let request = build_service_control(
        &published.maintainer_keys,
        &coordinator.public_key(),
        &repo_coordinate(&published),
        true,
        now - 900,
    )?;
    harness
        .publish_ci_events(&ci_relay, std::slice::from_ref(&request))
        .await?;

    // One contributor, one clone, six proposals: the listing is what is
    // under test, not the sending.
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "list-ci contributor".to_string(),
            },
        )
        .await?;

    let subjects = [
        "passing and requested",
        "passing but unknown",
        "failing",
        "running",
        "stale",
        "no ci",
        "neutral",
        "skipped",
    ];
    let mut prs: Vec<PublishedPr> = Vec::with_capacity(subjects.len());
    for (index, subject) in subjects.iter().enumerate() {
        prs.push(
            harness
                .publish_pr_in_clone(
                    &contributor,
                    &published,
                    PublishPrOpts {
                        branch: Some(format!("ci-row-{index}")),
                        commits: vec![(format!("row-{index}.md"), "some content\n".to_string())],
                        title: (*subject).to_string(),
                        description: format!("{subject} description"),
                        in_reply_to: Vec::new(),
                    },
                )
                .await
                .with_context(|| format!("publishing the {subject:?} proposal"))?,
        );
    }

    let spec = |pr: &PublishedPr, run_id: &str| {
        CiRunSpec::new(
            &published,
            run_id,
            pr.tip.clone(),
            CiTrigger::pull_request(pr),
            now,
        )
        .workflow("ci.yml", workflow_hash(WORKFLOW))
        .started_at(now - 300)
    };

    // ✓ — passing, and covered by the maintainer's standing request.
    harness
        .publish_ci_run(
            &ci_relay,
            &coordinator,
            &spec(&prs[0], "run-requested").provenance(CiProvenance::service_request(&request)),
        )
        .await?;

    // ✓? — passing, but nobody asked this coordinator for anything.
    harness
        .publish_ci_run(&ci_relay, &stranger, &spec(&prs[1], "run-unknown"))
        .await?;

    // ✗ — failing. A failure is a prompt to look at any trust level, so this
    // one is from the requested coordinator.
    harness
        .publish_ci_run(
            &ci_relay,
            &coordinator,
            &spec(&prs[2], "run-failing")
                .conclusion(Some("failure"))
                .provenance(CiProvenance::service_request(&request)),
        )
        .await?;

    // … — an unexpired marker with no result.
    harness
        .publish_ci_run(
            &ci_relay,
            &coordinator,
            &spec(&prs[3], "run-running")
                .conclusion(None)
                .progress(CiProgress::running(now)),
        )
        .await?;

    // ~ — a marker whose expiry has passed and no result. A relay refuses an
    // already-expired event (NIP-40), and waiting for a live marker to expire
    // would be a wall-clock sleep: this is by definition a marker ngit
    // fetched while it was live and still holds, so the local cache is seeded
    // with the write the fetch would have made.
    let stale = build_ci_run(
        &coordinator,
        &spec(&prs[4], "run-stale")
            .conclusion(None)
            .progress(CiProgress::expired(now)),
    )?;
    for event in stale.all() {
        ngit::client::save_event_in_local_cache(publisher.dir(), &event)
            .await
            .context("failed to seed the expired progress marker into the local cache")?;
    }

    // `-` — the sixth proposal has no CI events at all.

    // ✓ — a workflow that concluded there was nothing to do. `neutral` and
    // `skipped` are green by the same predicate the merge gate reads, so
    // these rows render the pass a `--require-ci-trust` merge would allow.
    for (index, conclusion, run_id) in
        [(6, "neutral", "run-neutral"), (7, "skipped", "run-skipped")]
    {
        harness
            .publish_ci_run(
                &ci_relay,
                &coordinator,
                &spec(&prs[index], run_id)
                    .conclusion(Some(conclusion))
                    .provenance(CiProvenance::service_request(&request)),
            )
            .await?;
    }

    let (out, json) = pr_list(&publisher).await?;
    assert!(
        out.status.success(),
        "`ngit pr list` exited {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let passing = row_ci(&json, subjects[0]);
    assert_eq!(passing["state"], "concluded");
    assert_eq!(passing["conclusion"], "success");
    assert_eq!(
        passing["trust_floor_met"], true,
        "a maintainer's standing request meets the floor: {passing}"
    );
    assert_eq!(passing["classification"], "maintainer-directed");

    let unknown = row_ci(&json, subjects[1]);
    assert_eq!(unknown["state"], "concluded");
    assert_eq!(unknown["conclusion"], "success");
    assert_eq!(
        unknown["trust_floor_met"], false,
        "a pass signed only by an unknown signer is qualified: {unknown}"
    );
    assert_eq!(unknown["classification"], "no-known-context");

    let failing = row_ci(&json, subjects[2]);
    assert_eq!(failing["state"], "concluded");
    assert_eq!(failing["conclusion"], "failure");

    let running = row_ci(&json, subjects[3]);
    assert_eq!(running["state"], "running");
    assert_eq!(running["conclusion"], Value::Null);

    let stale_row = row_ci(&json, subjects[4]);
    assert_eq!(
        stale_row["state"], "stale",
        "an expired marker with no result is abandoned, not running: {stale_row}"
    );
    assert_eq!(stale_row["conclusion"], Value::Null);

    let none = row_ci(&json, subjects[5]);
    assert_eq!(none["state"], "none");
    assert_eq!(none["conclusion"], Value::Null);
    assert_eq!(none["trust_floor_met"], false);
    assert_eq!(
        none["classification"],
        Value::Null,
        "with no run there is nothing to classify — `no-known-context` would \
         be a claim about a signer that does not exist: {none}"
    );
    assert_eq!(
        none["coverage"],
        Value::Null,
        "and nothing was left unchecked: {none}"
    );
    assert_eq!(
        none["revision_matched"], true,
        "no CI at all is `no CI`, not `CI for something else`: {none}"
    );

    // The glyph itself is human output and unit-tested against these fields;
    // what the row must carry is a concluded, floor-meeting result, which is
    // exactly what the merge gate accepts for the same conclusions.
    for (index, conclusion) in [(6, "neutral"), (7, "skipped")] {
        let green = row_ci(&json, subjects[index]);
        assert_eq!(green["state"], "concluded", "{green}");
        assert_eq!(green["conclusion"], conclusion, "{green}");
        assert_eq!(
            green["trust_floor_met"], true,
            "a `{conclusion}` run the maintainer's standing request covers is \
             a pass that meets the floor: {green}"
        );
        assert_eq!(green["classification"], "maintainer-directed", "{green}");
    }

    Ok(())
}
