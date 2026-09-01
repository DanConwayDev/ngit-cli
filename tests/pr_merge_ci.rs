//! `ngit pr merge`'s CI gate: the blocked/allowed matrix for
//! `--require-ci-trust`, and the non-blocking warning without it.
//!
//! The gate reads the same projection `ngit ci status` and `ngit pr view`
//! render, so these tests assert on what a merge *did*: the exit status, the
//! JSON document, whether the default branch advanced, and whether a
//! kind-1631 applied status event reached the repository relay. The warning
//! text itself is never asserted on — its presence is a JSON field.
//!
//! CI fixture events are signed by [`test_harness::ci`] and published
//! straight to a relay the repository announcement lists, so everything a
//! test asserts on is queryable the moment the publish returns — no sleeps,
//! no polling. The one exception is the expired progress marker: a relay
//! refuses an already-expired event, so it is seeded into the local cache the
//! fetch would have written to.

use anyhow::{Context, Result, bail};
use nostr::prelude::{Keys, SingleLetterTag, Timestamp};
use nostr_sdk::prelude::{Event, Filter, Kind};
use serde_json::Value;
use test_harness::{
    CiProgress, CiProvenance, CiRunSpec, CiTrigger, CloneLogin, Harness, KIND_PULL_REQUEST_UPDATE,
    PublishPrOpts, PublishRepoOpts, PublishedPr, PublishedRepo, Repo, build_ci_run,
    build_manual_trigger, build_service_control, repo_coordinate, workflow_hash,
};

const WORKFLOW: &str = "name: ci\non: push\n";

struct Arranged {
    harness: Harness,
    /// The maintainer's clone: the only identity `ngit pr merge` accepts.
    publisher: Repo,
    /// One contributor clone, reused for every PR a test needs.
    contributor: Repo,
    published: PublishedRepo,
    /// A relay listed on the announcement, and therefore queried by ngit's
    /// repository fetch. CI fixtures are published here.
    ci_relay: String,
    coordinator: Keys,
    now: u64,
}

/// A repository whose seed commit contains the workflow file at `ci.yml`, so
/// the integrity check has a blob to hash, plus a contributor ready to open
/// PRs against it.
async fn arrange(identifier: &str) -> Result<Arranged> {
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
            identifier: Some(identifier.to_string()),
            initial_file: Some(("ci.yml".to_string(), WORKFLOW.to_string())),
            extra_repo_relays: vec![ci_relay.clone()],
            ..Default::default()
        })
        .await?;
    publisher
        .git_ok(
            ["config", "--local", "nostr.auto-pr-branches", "true"],
            "enable automatic PR branches for CI merge fixtures",
        )
        .await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "merge gate contributor".to_string(),
            },
        )
        .await?;

    Ok(Arranged {
        harness,
        publisher,
        contributor,
        published,
        ci_relay,
        coordinator: Keys::generate(),
        now: Timestamp::now().as_secs(),
    })
}

impl Arranged {
    async fn open_pr(&self, branch: &str) -> Result<PublishedPr> {
        self.harness
            .publish_pr_in_clone(
                &self.contributor,
                &self.published,
                PublishPrOpts {
                    branch: Some(branch.to_string()),
                    commits: vec![(format!("{branch}.md"), "some content\n".to_string())],
                    title: format!("pr {branch}"),
                    description: "body".to_string(),
                    in_reply_to: Vec::new(),
                },
            )
            .await
            .with_context(|| format!("failed to publish the {branch} PR"))
    }

    /// Supersede `pr`'s revision: one more commit on its branch, published
    /// as a kind-1619 update. What CI already described becomes an earlier
    /// revision's result.
    async fn add_revision(&self, pr: &PublishedPr) -> Result<Event> {
        self.contributor
            .git_ok(["checkout", &pr.branch_name], "git checkout the pr branch")
            .await?;
        let file = format!("{}-revised.md", pr.branch_name);
        std::fs::write(self.contributor.dir().join(&file), "revised\n")
            .with_context(|| format!("failed to write {file}"))?;
        self.contributor.git_ok(["add", &file], "git add").await?;
        self.contributor
            .git_ok(["commit", "-m", &file, "--no-gpg-sign"], "git commit")
            .await?;
        let out = self
            .contributor
            .ngit([
                "send",
                "--defaults",
                "--force-pr",
                "--in-reply-to",
                &pr.event_id.to_hex(),
            ])
            .output()
            .await
            .context("failed to spawn `ngit send --in-reply-to`")?;
        if !out.status.success() {
            bail!(
                "`ngit send --in-reply-to` exited {:?}\nstdout: {}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        self.contributor
            .git_ok(["checkout", "main"], "git checkout main")
            .await?;

        // A PR update carries no branch name: it is found through the `E`
        // tag that points back at the PR it revises.
        self.harness
            .grasp("repo")
            .events(
                Filter::new()
                    .kind(KIND_PULL_REQUEST_UPDATE)
                    .custom_tag(SingleLetterTag::UPPERCASE_E, pr.event_id),
            )
            .await?
            .into_iter()
            .next()
            .context("no PR update event after `ngit send --in-reply-to`")
    }

    /// A confirmed maintainer's standing Service Request for the
    /// coordinator, signed well before any run in these tests starts.
    async fn service_request(&self) -> Result<Event> {
        let request = build_service_control(
            &self.published.maintainer_keys,
            &self.coordinator.public_key(),
            &repo_coordinate(&self.published),
            true,
            self.now - 900,
        )?;
        self.harness
            .publish_ci_events(&self.ci_relay, std::slice::from_ref(&request))
            .await?;
        Ok(request)
    }

    /// A concluded run of `pr`'s current revision, signed by `coordinator`.
    fn run_spec(&self, pr: &PublishedPr, run_id: &str, conclusion: &str) -> CiRunSpec {
        CiRunSpec::new(
            &self.published,
            run_id,
            pr.tip.clone(),
            CiTrigger::pull_request(pr),
            self.now,
        )
        .workflow("ci.yml", workflow_hash(WORKFLOW))
        .started_at(self.now - 300)
        .conclusion(Some(conclusion))
    }

    async fn publish_run(&self, coordinator: &Keys, spec: &CiRunSpec) -> Result<()> {
        self.harness
            .publish_ci_run(&self.ci_relay, coordinator, spec)
            .await?;
        Ok(())
    }
}

/// Run `ngit pr merge <id> [extra…] --json` and parse stdout. A refused gate
/// still emits the document, so this parses whatever the exit status.
async fn pr_merge(repo: &Repo, id: &str, extra: &[&str]) -> Result<(std::process::Output, Value)> {
    let mut argv: Vec<&str> = vec!["pr", "merge", id];
    argv.extend_from_slice(extra);
    argv.push("--json");
    let out = repo
        .ngit(&argv)
        .output()
        .await
        .context("failed to spawn `ngit pr merge`")?;
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

/// The applied (kind-1631) status events on the repository relay naming
/// `proposal` — what a completed `ngit pr merge` publishes.
async fn applied_status_events(harness: &Harness, pr: &PublishedPr) -> Result<Vec<Event>> {
    harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitStatusApplied)
                .event(pr.event_id),
        )
        .await
}

/// Number of parents of `<rev>` — a no-ff merge commit has 2.
async fn parent_count(repo: &Repo, rev: &str) -> Result<usize> {
    let out = repo
        .git(["rev-list", "--parents", "-n", "1", rev])
        .output()
        .await
        .with_context(|| format!("failed to spawn git rev-list {rev}"))?;
    if !out.status.success() {
        bail!("git rev-list {rev} exited {:?}", out.status);
    }
    let line = String::from_utf8(out.stdout).context("git rev-list stdout not utf-8")?;
    Ok(line.split_whitespace().count().saturating_sub(1))
}

/// Assert that nothing about the merge happened: the default branch is where
/// it was, and no applied status event exists for the PR.
async fn assert_not_merged(arranged: &Arranged, pr: &PublishedPr, main_before: &str) -> Result<()> {
    assert_eq!(
        arranged.publisher.rev_parse("main").await?,
        main_before,
        "a refused merge must not advance the default branch",
    );
    let applied = applied_status_events(&arranged.harness, pr).await?;
    assert!(
        applied.is_empty(),
        "a refused merge must publish no applied status event, found {}",
        applied.len(),
    );
    Ok(())
}

/// Assert the merge completed: a two-parent merge commit on the default
/// branch, and an applied status event on the repository relay.
async fn assert_merged(arranged: &Arranged, pr: &PublishedPr, main_before: &str) -> Result<()> {
    let main_after = arranged.publisher.rev_parse("main").await?;
    assert_ne!(
        main_after, main_before,
        "the merge commit should advance main"
    );
    assert_eq!(
        parent_count(&arranged.publisher, "main").await?,
        2,
        "a no-ff merge produces a two-parent merge commit",
    );
    let applied = applied_status_events(&arranged.harness, pr).await?;
    assert_eq!(
        applied.len(),
        1,
        "a completed merge publishes exactly one applied status event",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Blocked: `--require-ci-trust` refuses, and nothing is merged.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn require_ci_trust_blocks_a_merge_the_current_result_cannot_support() -> Result<()> {
    let arranged = arrange("merge-blocked").await?;
    let request = arranged.service_request().await?;

    // (a) a failing run, at the strongest trust there is: a failure is a
    //     prompt to look whatever signed it.
    let failing = arranged.open_pr("failing").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged
                .run_spec(&failing, "run-failing", "failure")
                .provenance(CiProvenance::service_request(&request)),
        )
        .await?;

    // (b) a passing run whose coordinator nobody asked for.
    let unknown = arranged.open_pr("unknown").await?;
    arranged
        .publish_run(
            &Keys::generate(),
            &arranged.run_spec(&unknown, "run-unknown", "success"),
        )
        .await?;

    // (c) an expired progress marker and no result. A relay refuses an
    //     already-expired event, so it is seeded into the cache the fetch
    //     would have written to — the state is by definition a marker ngit
    //     fetched while it was live and still holds.
    let stale = arranged.open_pr("stale").await?;
    let stale_events = build_ci_run(
        &arranged.coordinator,
        &arranged
            .run_spec(&stale, "run-stale", "success")
            .conclusion(None)
            .progress(CiProgress::expired(arranged.now)),
    )?;
    for event in stale_events.all() {
        ngit::client::save_event_in_local_cache(arranged.publisher.dir(), &event)
            .await
            .context("failed to seed the expired progress marker into the local cache")?;
    }

    // (d) no CI at all: a caller that demanded a floor is refused, because
    //     there is no result to meet it.
    let nothing = arranged.open_pr("nothing").await?;

    // Fetch the PR objects so every refusal below is the gate's doing rather
    // than a merge that could not have run anyway.
    arranged
        .publisher
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;

    // `expected_runs` is what the refusal document must still carry: the
    // runs that explain it, as `ci status`'s refusal does. Only the PR with
    // no CI has none.
    for (pr, state, reason_fragment, expected_runs, refuses_offline) in [
        (&failing, "concluded", "failure", 1, false),
        (&unknown, "concluded", "No known context", 1, true),
        (&stale, "stale", "stale", 1, false),
        (&nothing, "none", "none", 0, false),
    ] {
        let main_before = arranged.publisher.rev_parse("main").await?;
        let (out, json) = pr_merge(
            &arranged.publisher,
            &pr.event_id.to_hex(),
            &["--require-ci-trust", "operationally-associated"],
        )
        .await?;

        assert!(
            !out.status.success(),
            "`ngit pr merge --require-ci-trust` must exit non-zero for the \
             {} PR: {json}",
            pr.branch_name,
        );
        assert_eq!(json["status"], "error", "{json}");
        assert_eq!(json["ci"]["state"], state, "{json}");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|error| error.contains(reason_fragment)),
            "the refusal names why the {} PR was refused: {json}",
            pr.branch_name,
        );
        assert_eq!(
            json["ci_warning"],
            Value::Null,
            "a demanded floor produces a refusal, never a warning: {json}"
        );
        assert_eq!(
            json["ci"]["runs"].as_array().map(Vec::len),
            Some(expected_runs),
            "the refusal keeps the runs that explain it: {json}"
        );
        assert_not_merged(&arranged, pr, &main_before).await?;

        // The cache tier skips the domain ladder and every relay query, so
        // its coverage is partial — and partial coverage is missing
        // evidence, never evidence of trust. A signer with no known context
        // is refused offline exactly as it is online.
        if refuses_offline {
            let (out, json) = pr_merge(
                &arranged.publisher,
                &pr.event_id.to_hex(),
                &[
                    "--offline",
                    "--require-ci-trust",
                    "operationally-associated",
                ],
            )
            .await?;
            assert!(
                !out.status.success(),
                "the cache tier must not upgrade an unknown signer: {json}"
            );
            assert_eq!(json["ci"]["coverage"], "partial", "{json}");
            assert_not_merged(&arranged, pr, &main_before).await?;
        }

        // The control: the very same merge, with no floor demanded, goes
        // through. Without this a refusal could as easily be a merge that
        // could never have run.
        let (out, json) = pr_merge(&arranged.publisher, &pr.event_id.to_hex(), &[]).await?;
        assert!(
            out.status.success(),
            "the {} PR must merge once no floor is demanded: {json}\nstderr: {}",
            pr.branch_name,
            String::from_utf8_lossy(&out.stderr),
        );
        assert_merged(&arranged, pr, &main_before).await?;
    }

    Ok(())
}

#[tokio::test]
async fn a_workflow_that_never_concluded_blocks_the_merge_beside_a_successful_one() -> Result<()> {
    let arranged = arrange("merge-mixed").await?;
    // Both runs below are the same coordinator's, covered by this standing
    // request: the target's trust is the strongest there is, so the only
    // thing the gate can be refusing on is the unfinished workflow.
    arranged.service_request().await?;

    let pr = arranged.open_pr("mixed").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&pr, "run-passed", "success"),
        )
        .await?;

    // A second workflow of the *same* revision that stopped renewing its
    // marker and never published a result. Seeded into the local cache for
    // the reason the module comment gives: a relay refuses an expired event.
    let abandoned = build_ci_run(
        &arranged.coordinator,
        &arranged
            .run_spec(&pr, "run-abandoned", "success")
            .workflow("lint.yml", workflow_hash(WORKFLOW))
            .conclusion(None)
            .progress(CiProgress::expired(arranged.now)),
    )?;
    for event in abandoned.all() {
        ngit::client::save_event_in_local_cache(arranged.publisher.dir(), &event)
            .await
            .context("failed to seed the expired progress marker into the local cache")?;
    }

    arranged
        .publisher
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;

    let id = pr.event_id.to_hex();
    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(
        &arranged.publisher,
        &id,
        &["--require-ci-trust", "operationally-associated"],
    )
    .await?;

    assert!(
        !out.status.success(),
        "one workflow that never completed is not a green target, whatever \
         the workflow beside it reported: {json}",
    );
    assert_eq!(json["status"], "error", "{json}");
    assert_eq!(
        json["ci"]["state"], "stale",
        "the target has not concluded while one of its current runs never \
         did: {json}"
    );
    assert_eq!(
        json["ci"]["conclusion"],
        Value::Null,
        "the surviving success must not be rolled up into the target's \
         conclusion: {json}"
    );
    let runs = json["ci"]["runs"]
        .as_array()
        .context("the refusal document carries the runs that explain it")?;
    assert_eq!(runs.len(), 2, "{json}");
    assert!(
        runs.iter()
            .any(|run| run["state"] == "concluded" && run["conclusion"] == "success"),
        "the successful run is still reported, per run: {json}"
    );
    assert!(
        runs.iter().any(|run| run["state"] == "stale"),
        "so is the one that never concluded: {json}"
    );
    assert!(
        runs.iter()
            .all(|run| run["classification"] == "maintainer-directed"),
        "the refusal is about the unfinished run, not about trust: {json}"
    );
    assert_not_merged(&arranged, &pr, &main_before).await?;

    // `ngit ci status` reads the same state machine, so it refuses the same
    // target rather than reporting a concluded success.
    let out = arranged
        .publisher
        .ngit(vec![
            "ci",
            "status",
            id.as_str(),
            "--require-ci-trust",
            "operationally-associated",
            "--json",
        ])
        .output()
        .await
        .context("failed to spawn `ngit ci status`")?;
    let status: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .context("`ngit ci status` stdout is not valid JSON")?;
    assert!(
        !out.status.success(),
        "the surfaces must not disagree about what has concluded: {status}"
    );
    assert_eq!(status["ci"]["state"], "stale", "{status}");

    // The control: with no floor demanded the merge goes through, warned
    // about — so the refusal above is the gate's doing and not a merge that
    // could never have run.
    let (out, json) = pr_merge(&arranged.publisher, &id, &[]).await?;
    assert!(
        out.status.success(),
        "the warning is non-blocking: {json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        json["ci_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("stale")),
        "merging past a workflow that never finished is not silent: {json}"
    );
    assert_merged(&arranged, &pr, &main_before).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Allowed: maintainer direction the gate accepts, by either route.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn require_ci_trust_allows_a_merge_backed_by_maintainer_direction() -> Result<()> {
    let arranged = arrange("merge-allowed").await?;

    // The two routes to maintainer direction are kept apart, so each row
    // exercises the one it names: the covered runs cite no frozen quote and
    // rest on the control history alone, and the triggered run's
    // coordinator is a stranger no standing request covers.
    let request = arranged.service_request().await?;

    // (e) covered by the control history: a confirmed maintainer's Service
    //     Request was standing when the run started.
    let covered = arranged.open_pr("covered").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&covered, "run-covered", "success"),
        )
        .await?;

    // The same coverage read from the local cache alone: control reduction
    // needs no relay and no NIP-05, so `--offline` reaches the same verdict
    // with coverage left partial.
    let cached = arranged.open_pr("cached").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&cached, "run-cached", "success"),
        )
        .await?;

    // (f) run-scoped direction: a Manual Trigger authorizing exactly this
    //     run, signed by the repository's confirmed maintainer. Its
    //     coordinator is a fresh key the standing request above does not
    //     name, so the validated quote is the only evidence there is.
    let triggered = arranged.open_pr("triggered").await?;
    let trigger_coordinator = Keys::generate();
    let triggered_spec = arranged.run_spec(&triggered, "run-triggered", "success");
    let trigger = build_manual_trigger(
        &arranged.published.maintainer_keys,
        &trigger_coordinator.public_key(),
        &triggered_spec,
        arranged.now - 600,
    )?;
    // A pull-request Manual Trigger addresses the coordinator and nobody
    // else: the NIP-22 context it carries excludes the participant `p`, and a
    // coordinator rejects a request naming a further party. Pinned on the
    // fixture because the maintainer-directed row below is only evidence that
    // ngit accepts a *conformant* trigger if the trigger is one.
    let p_tags: Vec<&[String]> = trigger
        .tags
        .iter()
        .map(nostr::prelude::Tag::as_slice)
        .filter(|tag| tag.first().map(String::as_str) == Some("p"))
        .collect();
    assert_eq!(
        p_tags.len(),
        1,
        "the only `p` is the coordinator: {p_tags:?}"
    );
    assert_eq!(p_tags[0][1], trigger_coordinator.public_key().to_hex());
    arranged
        .harness
        .publish_ci_events(&arranged.ci_relay, std::slice::from_ref(&trigger))
        .await?;
    arranged
        .publish_run(
            &trigger_coordinator,
            &triggered_spec.provenance(CiProvenance::manual_trigger(&trigger)),
        )
        .await?;
    // The request exists for `covered`; it must not be what carries this row.
    assert_ne!(
        trigger_coordinator.public_key(),
        request.pubkey,
        "the manual-trigger row needs a coordinator the control history \
         does not cover",
    );

    arranged
        .publisher
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;

    for (pr, floor, offline) in [
        (&covered, "operationally-associated", false),
        (&cached, "operationally-associated", true),
        (&triggered, "maintainer-directed", false),
    ] {
        let main_before = arranged.publisher.rev_parse("main").await?;
        let mut args = vec!["--require-ci-trust", floor];
        if offline {
            args.push("--offline");
        }
        let (out, json) = pr_merge(&arranged.publisher, &pr.event_id.to_hex(), &args).await?;

        assert!(
            out.status.success(),
            "a maintainer-directed success meets the {floor} floor: {json}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(json["status"], "ok", "{json}");
        assert_eq!(json["ci"]["conclusion"], "success", "{json}");
        assert_eq!(
            json["ci"]["runs"][0]["classification"], "maintainer-directed",
            "{json}"
        );
        assert_eq!(json["ci_warning"], Value::Null, "{json}");
        assert!(
            json["event"].is_string(),
            "a completed merge reports the applied status event it published: {json}"
        );
        if offline {
            assert_eq!(
                json["ci"]["coverage"], "partial",
                "the cache tier skips the domain ladder, so it settles \
                 partial — which withholds no evidence the control history \
                 already established: {json}"
            );
        }
        assert_merged(&arranged, pr, &main_before).await?;
    }

    Ok(())
}

#[tokio::test]
async fn a_neutral_or_skipped_conclusion_is_merged_rather_than_refused_or_warned_about()
-> Result<()> {
    let arranged = arrange("merge-green").await?;
    // Both runs rest on this standing request through the control history,
    // so the only thing the gate and the warning can be reacting to is the
    // conclusion.
    arranged.service_request().await?;

    let neutral = arranged.open_pr("neutral").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&neutral, "run-neutral", "neutral"),
        )
        .await?;

    let skipped = arranged.open_pr("skipped").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&skipped, "run-skipped", "skipped"),
        )
        .await?;

    arranged
        .publisher
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;

    // A workflow that concluded there was nothing to do is green: the floor
    // it meets is its trust's, and the conclusion is no obstacle.
    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(
        &arranged.publisher,
        &neutral.event_id.to_hex(),
        &["--require-ci-trust", "operationally-associated"],
    )
    .await?;
    assert!(
        out.status.success(),
        "a `neutral` conclusion must not block a merge the same result \
         renders as a pass elsewhere: {json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(json["status"], "ok", "{json}");
    assert_eq!(json["ci"]["state"], "concluded", "{json}");
    assert_eq!(json["ci"]["conclusion"], "neutral", "{json}");
    assert_eq!(
        json["ci"]["runs"][0]["classification"], "maintainer-directed",
        "{json}"
    );
    assert_eq!(json["ci_warning"], Value::Null, "{json}");
    assert_merged(&arranged, &neutral, &main_before).await?;

    // `ngit ci status` reads the same predicate, so it admits the same
    // target rather than refusing what the merge allowed.
    let id = skipped.event_id.to_hex();
    let out = arranged
        .publisher
        .ngit(vec![
            "ci",
            "status",
            id.as_str(),
            "--require-ci-trust",
            "operationally-associated",
            "--json",
        ])
        .output()
        .await
        .context("failed to spawn `ngit ci status`")?;
    let status: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .context("`ngit ci status` stdout is not valid JSON")?;
    assert!(
        out.status.success(),
        "the surfaces must not disagree about what is green: {status}"
    );
    assert_eq!(status["ci"]["conclusion"], "skipped", "{status}");

    // And with no floor demanded there is nothing to say about it: a green
    // result is not a failing, unfinished or weakly-signed one.
    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(&arranged.publisher, &id, &[]).await?;
    assert!(
        out.status.success(),
        "{json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(json["ci"]["conclusion"], "skipped", "{json}");
    assert_eq!(
        json["ci_warning"],
        Value::Null,
        "a `skipped` conclusion is a pass, so merging past it is not warned \
         about: {json}"
    );
    assert_merged(&arranged, &skipped, &main_before).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Warn: without the flag a shortfall is reported, and the merge proceeds.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_the_flag_a_failing_result_warns_and_the_merge_proceeds() -> Result<()> {
    let arranged = arrange("merge-warn").await?;

    let failing = arranged.open_pr("failing").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&failing, "run-failing", "failure"),
        )
        .await?;

    // A run still executing: the same "has not concluded" shortfall the gate
    // refuses on, so merging while CI is in flight is not silent either.
    let running = arranged.open_pr("running").await?;
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged
                .run_spec(&running, "run-running", "success")
                .conclusion(None)
                .progress(CiProgress::running(arranged.now)),
        )
        .await?;

    // A PR nothing ran CI for: no result is failing, unfinished or weakly
    // signed, so there is nothing to warn about.
    let quiet = arranged.open_pr("quiet").await?;

    arranged
        .publisher
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;

    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(&arranged.publisher, &failing.event_id.to_hex(), &[]).await?;
    assert!(
        out.status.success(),
        "without --require-ci-trust a failing result is a warning, not a \
         refusal: {json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(json["status"], "ok", "{json}");
    assert_eq!(json["ci"]["conclusion"], "failure", "{json}");
    assert!(
        json["ci_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("failure")),
        "the warning is carried as a field, not left to the printed text: {json}"
    );
    assert_merged(&arranged, &failing, &main_before).await?;

    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(&arranged.publisher, &running.event_id.to_hex(), &[]).await?;
    assert!(out.status.success(), "{json}");
    assert_eq!(json["ci"]["state"], "running", "{json}");
    assert!(
        json["ci_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("running")),
        "merging while CI is still in flight is warned about: {json}"
    );
    assert_merged(&arranged, &running, &main_before).await?;

    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(&arranged.publisher, &quiet.event_id.to_hex(), &[]).await?;
    assert!(out.status.success(), "{json}");
    assert_eq!(json["ci"]["state"], "none", "{json}");
    assert_eq!(
        json["ci_warning"],
        Value::Null,
        "a PR with no CI at all is not warned about: {json}"
    );
    assert_merged(&arranged, &quiet, &main_before).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// A PR whose only CI describes a superseded revision: `none`, but not the
// `none` that means nobody asked.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ci_for_a_superseded_revision_warns_rather_than_merging_in_silence() -> Result<()> {
    let arranged = arrange("merge-superseded").await?;

    let pr = arranged.open_pr("revised").await?;
    // The run describes the PR root, which a second revision then supersedes.
    arranged
        .publish_run(
            &arranged.coordinator,
            &arranged.run_spec(&pr, "run-superseded", "success"),
        )
        .await?;
    arranged.add_revision(&pr).await?;

    arranged
        .publisher
        .git_ok(["fetch", "origin"], "git fetch origin")
        .await?;

    let main_before = arranged.publisher.rev_parse("main").await?;
    let (out, json) = pr_merge(&arranged.publisher, &pr.event_id.to_hex(), &[]).await?;
    assert!(
        out.status.success(),
        "the warning is non-blocking: {json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        json["ci"]["state"], "none",
        "an earlier revision's result is never presented as current: {json}"
    );
    assert_eq!(
        json["ci"]["revision_matched"], false,
        "CI exists for this PR, just not for what is being merged: {json}"
    );
    assert!(
        json["ci_warning"].is_string(),
        "a result that describes something other than the merged revision \
         must not be indistinguishable from no CI at all: {json}"
    );
    assert_merged(&arranged, &pr, &main_before).await?;

    Ok(())
}
