//! `ngit pr view`'s Checks section: the current revision's runs, the earlier
//! revisions' runs grouped as outdated, and the per-signer trust labels —
//! including a job delegated to a separate compute provider.
//!
//! CI fixture events are signed by [`test_harness::ci`] and published
//! straight to a relay the repository announcement lists, so everything a
//! test asserts on is queryable the moment the publish returns — no sleeps,
//! no polling.
//!
//! Assertions are on the JSON document and the exit status only. The human
//! rendering is deliberately not asserted on.

use anyhow::{Context, Result, bail};
use nostr::prelude::{
    EventId, FromBech32, Keys, SingleLetterTag, Timestamp, ToBech32, nip19::Nip19Event,
};
use nostr_sdk::prelude::{Event, Filter, Kind};
use serde_json::Value;
use test_harness::{
    CiJob, CiProvenance, CiRunSpec, CiTrigger, CloneLogin, Harness, KIND_PULL_REQUEST,
    KIND_PULL_REQUEST_UPDATE, PublishPrOpts, PublishRepoOpts, PublishedPr, PublishedRepo, Repo,
    build_manual_trigger, build_service_control, event_branch_name_tag, repo_coordinate,
    workflow_hash,
};

const WORKFLOW: &str = "name: ci\non: push\n";

struct Arranged {
    harness: Harness,
    publisher: Repo,
    published: PublishedRepo,
    /// A relay listed on the announcement, and therefore queried by ngit's
    /// repository fetch. CI fixtures are published here.
    ci_relay: String,
    coordinator: Keys,
    now: u64,
}

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

    Ok(Arranged {
        harness,
        publisher,
        published,
        ci_relay,
        coordinator: Keys::generate(),
        now: Timestamp::now().as_secs(),
    })
}

/// Run `ngit pr view <id> [extra…] --json` and parse stdout.
async fn pr_view(repo: &Repo, id: &str, extra: &[&str]) -> Result<(std::process::Output, Value)> {
    let mut argv: Vec<&str> = vec!["pr", "view", id];
    argv.extend_from_slice(extra);
    argv.push("--json");
    ngit_json(repo, &argv).await
}

/// Run `ngit ci status <target> --json` and parse stdout.
async fn ci_status(repo: &Repo, target: &str) -> Result<(std::process::Output, Value)> {
    ngit_json(repo, &["ci", "status", target, "--json"]).await
}

async fn ngit_json(repo: &Repo, argv: &[&str]) -> Result<(std::process::Output, Value)> {
    let out = repo
        .ngit(argv)
        .output()
        .await
        .with_context(|| format!("failed to spawn `ngit {}`", argv.join(" ")))?;
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

async fn ngit_ok(repo: &Repo, args: &[&str]) -> Result<()> {
    let out = repo
        .ngit(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn `ngit {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`ngit {}` exited {:?}\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(())
}

/// The one event of `kind` on the repo grasp whose `branch-name` matches.
async fn find_by_branch(harness: &Harness, kind: Kind, branch: &str) -> Result<Event> {
    harness
        .grasp("repo")
        .events(Filter::new().kind(kind))
        .await?
        .into_iter()
        .find(|event| event_branch_name_tag(event).as_deref() == Some(branch))
        .with_context(|| format!("no kind-{} event for branch {branch}", kind.as_u16()))
}

async fn commit_file(repo: &Repo, name: &str, content: &str) -> Result<String> {
    std::fs::write(repo.dir().join(name), content)
        .with_context(|| format!("failed to write {name}"))?;
    repo.git_ok(["add", name], "git add").await?;
    repo.git_ok(["commit", "-m", name, "--no-gpg-sign"], "git commit")
        .await?;
    repo.rev_parse("HEAD").await
}

fn runs(json: &Value) -> &Vec<Value> {
    json["ci"]["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("no `ci.runs` array in {json}"))
}

fn outdated(json: &Value) -> &Vec<Value> {
    json["ci"]["outdated"]
        .as_array()
        .unwrap_or_else(|| panic!("no `ci.outdated` array in {json}"))
}

/// The event id inside a JSON `nevent`. Every id ngit emits in JSON is
/// bech32, and carries whatever relay hint the command had, so tests compare
/// the decoded id rather than the string.
fn event_id_of(value: &Value) -> EventId {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("expected a bech32 string, got {value}"));
    Nip19Event::from_bech32(raw)
        .unwrap_or_else(|error| panic!("{raw} is not a nevent: {error}"))
        .event_id
}

/// A PR with one superseded revision, and the commit each revision carries.
async fn two_revision_pr(
    arranged: &Arranged,
    branch: &str,
) -> Result<(PublishedPr, Event, String, String)> {
    let contributor = arranged
        .harness
        .clone_published_repo(
            &arranged.published,
            CloneLogin::AsContributor {
                display_name: "revision contributor".to_string(),
            },
        )
        .await?;

    contributor
        .git_ok(["checkout", "-b", branch], "git checkout -b")
        .await?;
    let first_tip = commit_file(&contributor, "one.md", "one\n").await?;
    ngit_ok(&contributor, &["send", "--defaults", "--force-pr"]).await?;
    let root = find_by_branch(&arranged.harness, KIND_PULL_REQUEST, branch).await?;

    let second_tip = commit_file(&contributor, "two.md", "two\n").await?;
    ngit_ok(
        &contributor,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &root.id.to_hex(),
        ],
    )
    .await?;
    // A PR update carries no branch name: it is found through the `E` tag
    // that points back at the PR it revises.
    let revision = arranged
        .harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(KIND_PULL_REQUEST_UPDATE)
                .custom_tag(SingleLetterTag::UPPERCASE_E, root.id),
        )
        .await?
        .into_iter()
        .next()
        .context("no PR update event after `ngit send --in-reply-to`")?;
    assert_ne!(revision.id, root.id);

    let pr = PublishedPr {
        event_id: root.id,
        author_pubkey: root.pubkey,
        branch_name: branch.to_string(),
        commits: vec![first_tip.clone()],
        tip: first_tip.clone(),
        root_event: root,
    };
    Ok((pr, revision, first_tip, second_tip))
}

#[tokio::test]
async fn checks_report_the_current_revision_and_group_earlier_ones_as_outdated() -> Result<()> {
    let arranged = arrange("view-revision").await?;
    let (pr, revision, first_tip, second_tip) = two_revision_pr(&arranged, "revised").await?;

    // A confirmed maintainer's standing request, signed before either run
    // started, is what makes both runs maintainer-directed.
    let request = build_service_control(
        &arranged.published.maintainer_keys,
        &arranged.coordinator.public_key(),
        &repo_coordinate(&arranged.published),
        true,
        arranged.now - 900,
    )?;
    arranged
        .harness
        .publish_ci_events(&arranged.ci_relay, std::slice::from_ref(&request))
        .await?;

    for (run_id, commit, trigger, conclusion) in [
        (
            "run-outdated",
            first_tip,
            CiTrigger::pull_request(&pr),
            "success",
        ),
        (
            "run-current",
            second_tip,
            CiTrigger::pull_request_revision(&pr, &revision),
            "failure",
        ),
    ] {
        let spec = CiRunSpec::new(&arranged.published, run_id, commit, trigger, arranged.now)
            .workflow("ci.yml", workflow_hash(WORKFLOW))
            .started_at(arranged.now - 300)
            .conclusion(Some(conclusion))
            .provenance(CiProvenance::service_request(&request));
        arranged
            .harness
            .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &spec)
            .await?;
    }

    let (out, json) = pr_view(&arranged.publisher, &pr.event_id.to_hex(), &[]).await?;
    assert!(
        out.status.success(),
        "`ngit pr view` exited {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    assert_eq!(
        json["ci"]["state"], "concluded",
        "the current revision's run decides the state: {json}"
    );
    assert_eq!(json["ci"]["conclusion"], "failure");
    assert_eq!(json["ci"]["revision_matched"], true);
    assert_eq!(runs(&json).len(), 1, "only the current revision: {json}");
    assert_eq!(runs(&json)[0]["run_id"], "run-current");
    assert_eq!(runs(&json)[0]["classification"], "maintainer-directed");

    assert_eq!(
        outdated(&json).len(),
        1,
        "the earlier revision's run is kept, under `outdated`: {json}"
    );
    let earlier = &outdated(&json)[0];
    assert_eq!(earlier["run_id"], "run-outdated");
    assert_eq!(
        earlier["conclusion"], "success",
        "an earlier revision's success never becomes the PR's state: {json}"
    );
    assert_eq!(
        event_id_of(&earlier["revision"]),
        pr.event_id,
        "an outdated run names the revision that supplied it: {json}"
    );
    assert!(
        earlier["classification"].is_string(),
        "an outdated run is still labelled: {json}"
    );

    Ok(())
}

#[tokio::test]
async fn a_delegated_job_and_a_signer_with_no_context_are_labelled_separately() -> Result<()> {
    let arranged = arrange("view-delegation").await?;
    let pr: PublishedPr = arranged
        .harness
        .publish_pr(
            &arranged.published,
            PublishPrOpts {
                branch: None,
                commits: Vec::new(),
                title: "a pr with delegated ci".to_string(),
                description: "body".to_string(),
                in_reply_to: Vec::new(),
            },
        )
        .await?;

    // A standing request from a confirmed maintainer gives the coordinator
    // identity-level context, which is what can reach a provider at all.
    let request = build_service_control(
        &arranged.published.maintainer_keys,
        &arranged.coordinator.public_key(),
        &repo_coordinate(&arranged.published),
        true,
        arranged.now - 900,
    )?;
    arranged
        .harness
        .publish_ci_events(&arranged.ci_relay, std::slice::from_ref(&request))
        .await?;

    let provider = Keys::generate();
    let delegated = CiRunSpec::new(
        &arranged.published,
        "run-delegated",
        pr.tip.clone(),
        CiTrigger::pull_request(&pr),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .started_at(arranged.now - 300)
    .provenance(CiProvenance::service_request(&request))
    .job(CiJob::new("build", "success", &arranged.coordinator))
    .job(CiJob::new("test", "success", &provider));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &delegated)
        .await?;

    // A second coordinator nobody asked for, running a workflow of its own.
    let stranger = Keys::generate();
    let unknown = CiRunSpec::new(
        &arranged.published,
        "run-unknown",
        pr.tip.clone(),
        CiTrigger::pull_request(&pr),
        arranged.now,
    )
    .workflow("release.yml", workflow_hash(WORKFLOW));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &stranger, &unknown)
        .await?;

    let (out, json) = pr_view(&arranged.publisher, &pr.event_id.to_hex(), &[]).await?;
    assert!(
        out.status.success(),
        "`ngit pr view` exited {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        runs(&json).len(),
        2,
        "both coordinators are current: {json}"
    );

    let coordinator_npub = arranged.coordinator.public_key().to_bech32()?;
    let directed = runs(&json)
        .iter()
        .find(|run| run["coordinator"] == coordinator_npub.as_str())
        .with_context(|| format!("no run from the requested coordinator in {json}"))?;
    assert_eq!(directed["classification"], "maintainer-directed");

    let jobs = directed["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 2, "both jobs are listed: {json}");
    let own = jobs
        .iter()
        .find(|job| job["job"] == "build")
        .expect("the coordinator's own job");
    assert_eq!(
        own["classification"], "maintainer-directed",
        "a job the coordinator signed itself carries the coordinator's own context: {json}"
    );
    let delegated_job = jobs
        .iter()
        .find(|job| job["job"] == "test")
        .expect("the delegated job");
    assert_eq!(
        delegated_job["provider"],
        provider.public_key().to_bech32()?
    );
    assert_eq!(
        delegated_job["classification"], "operationally-associated",
        "coordinator trust reaches a provider downgraded, and only for the \
         job the Workflow Result accepts: {json}"
    );

    let stranger_npub = stranger.public_key().to_bech32()?;
    let uncontextualised = runs(&json)
        .iter()
        .find(|run| run["coordinator"] == stranger_npub.as_str())
        .with_context(|| format!("no run from the second coordinator in {json}"))?;
    assert_eq!(
        uncontextualised["classification"], "no-known-context",
        "absence of evidence is reported as such, never as a finding: {json}"
    );

    assert_eq!(
        json["ci"]["coverage"], "partial",
        "an unsettled identity lookup leaves the view incomplete: {json}"
    );
    assert_eq!(
        outdated(&json).len(),
        0,
        "a PR with one revision has nothing outdated: {json}"
    );

    // The cache tier reads the same local events, skips the domain ladder
    // and every relay query, and still labels each signer from the control
    // history alone.
    let (out, offline) =
        pr_view(&arranged.publisher, &pr.event_id.to_hex(), &["--offline"]).await?;
    assert!(
        out.status.success(),
        "`ngit pr view --offline` exited {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(runs(&offline).len(), 2, "the cache tier has both runs");
    assert_eq!(
        offline["ci"]["coverage"], "partial",
        "the skipped domain ladder leaves the cache tier partial: {offline}"
    );
    let offline_directed = runs(&offline)
        .iter()
        .find(|run| run["coordinator"] == coordinator_npub.as_str())
        .with_context(|| format!("no run from the requested coordinator in {offline}"))?;
    assert_eq!(offline_directed["classification"], "maintainer-directed");
    assert_eq!(
        offline_directed["jobs"]
            .as_array()
            .and_then(|jobs| jobs.iter().find(|job| job["job"] == "test"))
            .map(|job| &job["classification"]),
        Some(&Value::String("operationally-associated".to_string())),
        "delegation is cache-computable: {offline}"
    );

    Ok(())
}

#[tokio::test]
async fn an_earlier_revisions_run_is_evidence_on_every_surface() -> Result<()> {
    let arranged = arrange("view-evidence").await?;
    let (pr, revision, first_tip, second_tip) = two_revision_pr(&arranged, "evidenced").await?;

    // The maintainer manually triggered the run of the *earlier* revision,
    // and nothing else: the coordinator has no standing request, so its only
    // context is that historical direction.
    let outdated_spec = CiRunSpec::new(
        &arranged.published,
        "run-outdated",
        first_tip,
        CiTrigger::pull_request(&pr),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .started_at(arranged.now - 600);
    let trigger = build_manual_trigger(
        &arranged.published.maintainer_keys,
        &arranged.coordinator.public_key(),
        &outdated_spec,
        arranged.now - 900,
    )?;
    arranged
        .harness
        .publish_ci_events(&arranged.ci_relay, std::slice::from_ref(&trigger))
        .await?;
    arranged
        .harness
        .publish_ci_run(
            &arranged.ci_relay,
            &arranged.coordinator,
            &outdated_spec.provenance(CiProvenance::manual_trigger(&trigger)),
        )
        .await?;

    // The current revision's run cites nothing: whatever context it has is
    // the coordinator's, earned by the earlier run.
    let current = CiRunSpec::new(
        &arranged.published,
        "run-current",
        second_tip,
        CiTrigger::pull_request_revision(&pr, &revision),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .started_at(arranged.now - 300);
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &current)
        .await?;

    let (out, view) = pr_view(&arranged.publisher, &pr.event_id.to_hex(), &[]).await?;
    assert!(out.status.success());
    assert_eq!(runs(&view).len(), 1);
    assert_eq!(runs(&view)[0]["run_id"], "run-current");
    assert_eq!(
        runs(&view)[0]["classification"],
        "operationally-associated",
        "a maintainer-directed run for an earlier revision is identity-level \
         evidence about its coordinator, downgraded to historical: {view}"
    );
    assert_eq!(
        outdated(&view)[0]["classification"],
        "maintainer-directed",
        "the earlier run itself keeps its run-scoped direction: {view}"
    );

    // The same signer, the same run, a different surface. `ci status` shows
    // only the current revision but assembles evidence from every run it
    // knows for the target, so the two must agree.
    let (out, status) = ci_status(&arranged.publisher, &pr.event_id.to_hex()).await?;
    assert!(out.status.success());
    assert_eq!(runs(&status).len(), 1);
    assert_eq!(
        runs(&status)[0]["classification"],
        runs(&view)[0]["classification"],
        "`ci status` and `pr view` must classify one signer identically: \
         {status} vs {view}"
    );
    assert!(
        status["ci"].get("outdated").is_none(),
        "`ci status` describes one revision and emits no outdated array: {status}"
    );

    Ok(())
}
