//! `ngit ci request|stop|trigger`: the maintainer controls that publish the
//! Level 1 evidence `ngit ci status` reads back.
//!
//! Assertions are on the events that reached the relay, the JSON document and
//! the exit status — never on the human rendering. Every published control is
//! round-tripped through `ngit::ci::kinds`, which is the same validation the
//! reading side applies, so a shape ngit would skip cannot pass these tests.
//!
//! Two of them close the loop the work package exists for: a Service Request
//! that covers a later run through the control history, and a Manual Trigger
//! a run quotes as its frozen provenance. Each isolates one route, so neither
//! can pass on the other's evidence.

use anyhow::{Context, Result, bail};
use ngit::ci::kinds::{validate_manual_trigger, validate_service_control};
use nostr::prelude::{EventId, FromBech32, Keys, Timestamp, ToBech32, nip19::Nip19Event};
use nostr_sdk::prelude::{Event, Filter, Kind};
use serde_json::Value;
use test_harness::{
    CiProvenance, CiRunSpec, CiTrigger, CloneLogin, Harness, PublishRepoOpts, PublishedRepo, Repo,
    repo_coordinate, workflow_hash,
};

const WORKFLOW: &str = "name: ci\non: push\n";

const KIND_SERVICE_REQUEST: Kind = Kind::Custom(9843);
const KIND_SERVICE_STOP: Kind = Kind::Custom(9844);
const KIND_MANUAL_TRIGGER: Kind = Kind::Custom(9840);

struct Arranged {
    harness: Harness,
    /// The maintainer's clone: a confirmed maintainer of the repository.
    publisher: Repo,
    published: PublishedRepo,
    /// A relay listed on the announcement, so ngit publishes repository
    /// events to it and its repository fetch reads them back.
    ci_relay: String,
    coordinator: Keys,
    now: u64,
}

/// A repository whose seed commit contains the workflow file at `ci.yml`, so
/// a Manual Trigger has a blob to hash.
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

impl Arranged {
    fn coordinator_npub(&self) -> Result<String> {
        Ok(self.coordinator.public_key().to_bech32()?)
    }

    /// Every event of `kind` that reached the announced CI relay.
    async fn published_events(&self, kind: Kind) -> Result<Vec<Event>> {
        self.harness
            .relay("ci")
            .events(Filter::new().kind(kind))
            .await
    }

    /// The one event of `kind` on the CI relay.
    async fn only_published(&self, kind: Kind) -> Result<Event> {
        let events = self.published_events(kind).await?;
        if events.len() != 1 {
            bail!(
                "expected exactly one kind-{} event on the CI relay, found {}",
                kind.as_u16(),
                events.len(),
            );
        }
        Ok(events.into_iter().next().expect("length checked"))
    }
}

/// Run `ngit <args> --json`, returning the exit status and the document.
async fn ngit_json(repo: &Repo, args: &[&str]) -> Result<(std::process::Output, Value)> {
    let mut argv: Vec<&str> = args.to_vec();
    argv.push("--json");
    let out = repo
        .ngit(&argv)
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

/// Run `ngit <args> --json`, requiring success.
async fn ngit_json_ok(repo: &Repo, args: &[&str]) -> Result<Value> {
    let (out, json) = ngit_json(repo, args).await?;
    if !out.status.success() {
        bail!(
            "`ngit {}` exited {:?} with document {json}",
            args.join(" "),
            out.status,
        );
    }
    Ok(json)
}

/// The event id inside a JSON `nevent`.
fn event_id_of(value: &Value) -> EventId {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("expected a bech32 string, got {value}"));
    Nip19Event::from_bech32(raw)
        .unwrap_or_else(|error| panic!("{raw} is not a nevent: {error}"))
        .event_id
}

/// Every value of the `name` tag, in event order.
fn tag_values(event: &Event, name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|first| first == name))
        .filter_map(|tag| tag.as_slice().get(1).cloned())
        .collect()
}

fn tag_slices(event: &Event, name: &str) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|first| first == name))
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

fn runs(json: &Value) -> &Vec<Value> {
    json["ci"]["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("no `ci.runs` array in {json}"))
}

#[tokio::test]
async fn request_publishes_one_service_request_for_the_repository_and_coordinator() -> Result<()> {
    let arranged = arrange("ci-request").await?;
    let coordinator = arranged.coordinator_npub()?;

    let json = ngit_json_ok(&arranged.publisher, &["ci", "request", &coordinator]).await?;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["action"], "service-requested");
    assert_eq!(json["entity"], "ci");
    assert_eq!(json["warning"], Value::Null, "a maintainer is not warned");

    let event = arranged.only_published(KIND_SERVICE_REQUEST).await?;
    assert_eq!(event_id_of(&json["event"]), event.id);
    assert_eq!(
        event.pubkey,
        arranged.published.maintainer_keys.public_key(),
        "the request is signed by the logged-in maintainer",
    );

    // The shape the reading side requires: empty content, exactly one `a`
    // naming this repository, exactly one `p` naming the coordinator.
    let control = validate_service_control(&event)
        .map_err(|reason| anyhow::anyhow!("published 9843 is malformed: {reason}"))?;
    assert!(control.is_request);
    assert_eq!(control.coordinator, arranged.coordinator.public_key());
    assert_eq!(
        control.repository.coordinate.to_string(),
        repo_coordinate(&arranged.published),
        "the perspective is the signer's own announcement",
    );
    assert!(event.content.is_empty());
    assert!(tag_values(&event, "d").is_empty());
    assert!(tag_values(&event, "expiration").is_empty());

    // Shape pinned against the NIP itself, not against ngit's own tolerant
    // reader: a Service Request's `a` MAY carry one relay hint, and `p` is
    // the bare coordinator pubkey.
    let a_tags = tag_slices(&event, "a");
    assert_eq!(a_tags.len(), 1);
    assert_eq!(
        a_tags[0].len(),
        3,
        "a Service Request's `a` carries the repository's relay hint: {a_tags:?}",
    );
    assert!(a_tags[0][2].starts_with("ws"), "the hint is a relay url");
    let p_tags = tag_slices(&event, "p");
    assert_eq!(p_tags.len(), 1);
    assert_eq!(p_tags[0].len(), 2, "`p` is the bare coordinator pubkey");
    Ok(())
}

#[tokio::test]
async fn stop_publishes_a_service_stop_with_the_same_shape() -> Result<()> {
    let arranged = arrange("ci-stop").await?;
    let coordinator = arranged.coordinator_npub()?;

    let json = ngit_json_ok(&arranged.publisher, &["ci", "stop", &coordinator]).await?;
    assert_eq!(json["action"], "service-stopped");

    let event = arranged.only_published(KIND_SERVICE_STOP).await?;
    assert_eq!(event_id_of(&json["event"]), event.id);
    assert!(
        arranged
            .published_events(KIND_SERVICE_REQUEST)
            .await?
            .is_empty(),
        "a stop publishes no request",
    );

    let control = validate_service_control(&event)
        .map_err(|reason| anyhow::anyhow!("published 9844 is malformed: {reason}"))?;
    assert!(!control.is_request);
    assert_eq!(control.coordinator, arranged.coordinator.public_key());
    assert_eq!(
        control.repository.coordinate.to_string(),
        repo_coordinate(&arranged.published),
    );
    Ok(())
}

#[tokio::test]
async fn a_non_maintainer_request_warns_and_still_publishes() -> Result<()> {
    let arranged = arrange("ci-request-stranger").await?;
    let coordinator = arranged.coordinator_npub()?;
    let contributor = arranged
        .harness
        .clone_published_repo(
            &arranged.published,
            CloneLogin::AsContributor {
                display_name: "ci requester".to_string(),
            },
        )
        .await?;

    // The NIP lets an operator accept requesters who are not maintainers, so
    // this is a caveat and never a refusal.
    let json = ngit_json_ok(&contributor, &["ci", "request", &coordinator]).await?;
    assert_eq!(json["status"], "ok");
    assert!(
        json["warning"].is_string(),
        "a non-maintainer requester is warned: {json}",
    );

    let event = arranged.only_published(KIND_SERVICE_REQUEST).await?;
    assert_eq!(event_id_of(&json["event"]), event.id);
    let control = validate_service_control(&event)
        .map_err(|reason| anyhow::anyhow!("published 9843 is malformed: {reason}"))?;
    assert_ne!(
        control.author,
        arranged.published.maintainer_keys.public_key()
    );
    assert_eq!(
        control.repository.coordinate.to_string(),
        repo_coordinate(&arranged.published),
        "a requester with no announcement asks about the resolved perspective",
    );
    Ok(())
}

#[tokio::test]
async fn trigger_publishes_the_hash_of_the_workflow_blob_at_the_commit() -> Result<()> {
    let arranged = arrange("ci-trigger").await?;
    let coordinator = arranged.coordinator_npub()?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    let json = ngit_json_ok(
        &arranged.publisher,
        &[
            "ci",
            "trigger",
            &coordinator,
            "--workflow",
            "ci.yml",
            "--ref",
            "refs/heads/main",
        ],
    )
    .await?;
    assert_eq!(json["action"], "triggered");
    assert_eq!(json["commit"], head);
    assert_eq!(json["workflow"], "ci.yml");
    assert_eq!(json["ref"], "refs/heads/main");

    let event = arranged.only_published(KIND_MANUAL_TRIGGER).await?;
    assert_eq!(event_id_of(&json["event"]), event.id);

    let trigger = validate_manual_trigger(&event)
        .map_err(|reason| anyhow::anyhow!("published 9840 is malformed: {reason}"))?;
    assert_eq!(trigger.addressed, vec![arranged.coordinator.public_key()]);
    assert_eq!(trigger.common.commits, vec![head.clone()]);
    assert_eq!(trigger.common.workflow.path, "ci.yml");
    // Computed here from the file's content, independently of ngit's read of
    // the blob at the commit.
    assert_eq!(trigger.common.workflow.sha256, workflow_hash(WORKFLOW));
    assert_eq!(json["workflow_hash"], workflow_hash(WORKFLOW));
    assert_eq!(trigger.common.git_ref(), Some("refs/heads/main"));
    assert!(
        tag_values(&event, "o").is_empty(),
        "a Manual Trigger carries no `o`: it is always normalized as manual",
    );
    assert_eq!(
        tag_values(&event, "a"),
        vec![repo_coordinate(&arranged.published)],
    );

    // The common tags are pinned against the NIP rather than against ngit's
    // own reader, which tolerates more than a coordinator does. In
    // particular a third element on `a` — a relay hint the NIP grants only
    // to a Service Request — makes a strict coordinator drop the request.
    let a_tags = tag_slices(&event, "a");
    assert!(
        a_tags.iter().all(|tag| tag.len() == 2),
        "a Manual Trigger's `a` tags carry no relay hint: {a_tags:?}",
    );
    let c_tags = tag_slices(&event, "c");
    assert!(
        c_tags.iter().all(|tag| tag.len() == 2),
        "`c` is the bare object id: {c_tags:?}",
    );
    let p_tags = tag_slices(&event, "p");
    assert_eq!(p_tags.len(), 1);
    assert_eq!(p_tags[0].len(), 2, "`p` is the bare coordinator pubkey");
    let w_tags = tag_slices(&event, "w");
    assert_eq!(w_tags.len(), 1);
    assert_eq!(w_tags[0].len(), 3, "`w` is path plus content hash");
    let r_tags = tag_slices(&event, "r");
    assert_eq!(r_tags.len(), 1);
    assert_eq!(r_tags[0].len(), 2, "`r` is the bare Git ref");
    Ok(())
}

#[tokio::test]
async fn a_ref_without_the_refs_prefix_publishes_nothing() -> Result<()> {
    let arranged = arrange("ci-trigger-ref").await?;
    let coordinator = arranged.coordinator_npub()?;

    let out = arranged
        .publisher
        .ngit([
            "ci",
            "trigger",
            &coordinator,
            "--workflow",
            "ci.yml",
            "--ref",
            "main",
        ])
        .output()
        .await
        .context("failed to spawn `ngit ci trigger`")?;
    assert!(
        !out.status.success(),
        "a bare branch name is not a Git ref, and the reading side gives a non-`refs/` `r` another meaning entirely",
    );
    assert!(
        arranged
            .published_events(KIND_MANUAL_TRIGGER)
            .await?
            .is_empty(),
    );
    Ok(())
}

#[tokio::test]
async fn offline_publishes_without_the_pre_publish_fetch() -> Result<()> {
    let arranged = arrange("ci-request-offline").await?;
    let coordinator = arranged.coordinator_npub()?;

    // `--offline` skips the repository fetch, never the publish: the event
    // still has to reach a relay for a coordinator to read it.
    let json = ngit_json_ok(
        &arranged.publisher,
        &["ci", "request", &coordinator, "--offline"],
    )
    .await?;
    assert_eq!(json["status"], "ok");

    let event = arranged.only_published(KIND_SERVICE_REQUEST).await?;
    assert_eq!(event_id_of(&json["event"]), event.id);
    let control = validate_service_control(&event)
        .map_err(|reason| anyhow::anyhow!("published 9843 is malformed: {reason}"))?;
    assert_eq!(control.coordinator, arranged.coordinator.public_key());
    assert_eq!(
        control.repository.coordinate.to_string(),
        repo_coordinate(&arranged.published),
    );
    Ok(())
}

#[tokio::test]
async fn an_annotated_tag_publishes_the_peeled_commit_first() -> Result<()> {
    let arranged = arrange("ci-trigger-tag").await?;
    let coordinator = arranged.coordinator_npub()?;
    let head = arranged.publisher.rev_parse("HEAD").await?;
    arranged
        .publisher
        .git_ok(["tag", "-a", "v1", "-m", "release v1"], "git tag -a")
        .await?;
    let tag_object = arranged.publisher.rev_parse("v1").await?;
    assert_ne!(tag_object, head, "an annotated tag is its own object");

    let json = ngit_json_ok(
        &arranged.publisher,
        &["ci", "trigger", &coordinator, "v1", "--workflow", "ci.yml"],
    )
    .await?;
    assert_eq!(json["commit"], head);

    let event = arranged.only_published(KIND_MANUAL_TRIGGER).await?;
    let trigger = validate_manual_trigger(&event)
        .map_err(|reason| anyhow::anyhow!("published 9840 is malformed: {reason}"))?;
    assert_eq!(
        trigger.common.commits,
        vec![head, tag_object],
        "the peeled commit first, then the tag object, so one `#c` query finds either",
    );
    Ok(())
}

#[tokio::test]
async fn an_unpushed_commit_warns_but_still_triggers() -> Result<()> {
    let arranged = arrange("ci-trigger-unpushed").await?;
    let coordinator = arranged.coordinator_npub()?;

    // A commit that never left this clone is not in the repository state a
    // coordinator resolves, which makes the request ineligible there.
    std::fs::write(arranged.publisher.dir().join("local.md"), "not pushed\n")
        .context("failed to write local.md")?;
    arranged
        .publisher
        .git_ok(["add", "local.md"], "git add")
        .await?;
    arranged
        .publisher
        .git_ok(["commit", "-m", "local", "--no-gpg-sign"], "git commit")
        .await?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    let json = ngit_json_ok(
        &arranged.publisher,
        &["ci", "trigger", &coordinator, "--workflow", "ci.yml"],
    )
    .await?;
    assert_eq!(json["commit"], head);
    assert!(
        json["warning"].is_string(),
        "a commit the repository state does not contain is a caveat: {json}",
    );
    // A caveat, never a refusal: the maintainer may be about to push.
    let event = arranged.only_published(KIND_MANUAL_TRIGGER).await?;
    assert_eq!(event_id_of(&json["event"]), event.id);
    Ok(())
}

#[tokio::test]
async fn a_workflow_missing_at_the_commit_publishes_nothing() -> Result<()> {
    let arranged = arrange("ci-trigger-missing").await?;
    let coordinator = arranged.coordinator_npub()?;

    let out = arranged
        .publisher
        .ngit(["ci", "trigger", &coordinator, "--workflow", "absent.yml"])
        .output()
        .await
        .context("failed to spawn `ngit ci trigger`")?;
    assert!(
        !out.status.success(),
        "a workflow that does not exist at the commit is an error, not a trigger for a file the coordinator cannot hash",
    );
    assert!(
        arranged
            .published_events(KIND_MANUAL_TRIGGER)
            .await?
            .is_empty(),
        "nothing is published when the workflow cannot be hashed",
    );
    Ok(())
}

#[tokio::test]
async fn a_published_request_covers_a_later_run_as_maintainer_directed() -> Result<()> {
    let arranged = arrange("ci-request-loop").await?;
    let coordinator = arranged.coordinator_npub()?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    // Level 1 evidence, published by ngit itself.
    let request = ngit_json_ok(&arranged.publisher, &["ci", "request", &coordinator]).await?;
    let request_id = event_id_of(&request["event"]);

    // A run the coordinator started after the request: coverage comes from
    // the control history, not from any quote — the run carries none.
    let started_at = Timestamp::now().as_secs() + 60;
    let spec = CiRunSpec::new(
        &arranged.published,
        "run-requested",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .started_at(started_at);
    let events = arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &spec)
        .await?;
    assert!(
        events.result.is_some_and(|result| result
            .tags
            .iter()
            .all(|tag| tag.as_slice().first().is_none_or(|first| first != "q"))),
        "the run quotes nothing, so only the control history can cover it",
    );

    let json = ngit_json_ok(&arranged.publisher, &["ci", "status", &head]).await?;
    let runs = runs(&json);
    assert_eq!(runs.len(), 1, "one run: {json}");
    assert_eq!(
        runs[0]["classification"], "maintainer-directed",
        "the request ngit published covers the run that started after it: {json}",
    );
    assert!(
        runs[0]["evidence"]
            .as_array()
            .is_some_and(|evidence| !evidence.is_empty()),
        "maintainer direction is evidence-backed: {json}",
    );
    assert!(
        !arranged
            .published_events(KIND_SERVICE_REQUEST)
            .await?
            .is_empty(),
        "the request stayed on the relay for id {request_id}",
    );
    Ok(())
}

#[tokio::test]
async fn a_run_quoting_a_published_trigger_is_maintainer_directed() -> Result<()> {
    let arranged = arrange("ci-trigger-loop").await?;
    let coordinator = arranged.coordinator_npub()?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    ngit_json_ok(
        &arranged.publisher,
        &[
            "ci",
            "trigger",
            &coordinator,
            "--workflow",
            "ci.yml",
            "--ref",
            "refs/heads/main",
        ],
    )
    .await?;
    let trigger = arranged.only_published(KIND_MANUAL_TRIGGER).await?;

    // The coordinator replays exactly what the trigger authorized and freezes
    // it as the run's provenance quote. No Service Request exists here, so
    // the control-history route cannot supply the classification. The run is
    // handed off after the trigger was signed: a trigger authorizes the run
    // that follows it, never one already under way.
    let spec = CiRunSpec::new(
        &arranged.published,
        "run-triggered",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .started_at(trigger.created_at.as_secs() + 60)
    .provenance(CiProvenance::manual_trigger(&trigger));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &spec)
        .await?;

    let json = ngit_json_ok(&arranged.publisher, &["ci", "status", &head]).await?;
    let runs = runs(&json);
    assert_eq!(runs.len(), 1, "one run: {json}");
    assert_eq!(
        runs[0]["classification"], "maintainer-directed",
        "the validated quote of ngit's own trigger is the run's provenance: {json}",
    );
    assert!(
        arranged
            .published_events(KIND_SERVICE_REQUEST)
            .await?
            .is_empty(),
        "no standing request exists: the classification is the trigger's",
    );

    // The `w` tag the coordinator would hash is the one ngit signed.
    let workflow = tag_slices(&trigger, "w")
        .into_iter()
        .next()
        .context("the trigger has a `w` tag")?;
    assert_eq!(
        workflow.get(2).map(String::as_str),
        Some(workflow_hash(WORKFLOW).as_str())
    );
    Ok(())
}
