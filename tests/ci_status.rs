//! `ngit ci status`: target resolution, the per-target CI state machine, the
//! local integrity check and the `--require-ci-trust` gate.
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
    CiJob, CiProgress, CiProvenance, CiRunSpec, CiTrigger, CloneLogin, Harness, KIND_PULL_REQUEST,
    KIND_PULL_REQUEST_UPDATE, PublishPrOpts, PublishRepoOpts, PublishedPr, PublishedRepo, Repo,
    build_ci_run, build_manual_trigger, build_service_control, event_branch_name_tag,
    repo_coordinate, workflow_hash,
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

/// A repository whose seed commit contains the workflow file at `ci.yml`, so
/// the integrity check has a blob to hash.
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

/// Run `ngit ci status <args> --json` and parse stdout.
async fn ci_status(repo: &Repo, args: &[&str]) -> Result<(std::process::Output, Value)> {
    let mut argv: Vec<&str> = vec!["ci", "status"];
    argv.extend_from_slice(args);
    argv.push("--json");
    let out = repo
        .ngit(&argv)
        .output()
        .await
        .context("failed to spawn `ngit ci status`")?;
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

fn nevent(event_id: EventId) -> Result<String> {
    Ok(Nip19Event {
        event_id,
        relays: Vec::new(),
        author: None,
        kind: None,
    }
    .to_bech32()?)
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

#[tokio::test]
async fn commit_ish_targets_resolve_and_carry_the_local_integrity_marker() -> Result<()> {
    let arranged = arrange("ci-commit").await?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    // Two coordinators run the same workflow file: one signs the hash the
    // blob in the commit really has, the other does not. Separate signers
    // keep both current — one attempt per (coordinator, workflow) is.
    let tamperer = Keys::generate();
    for (run_id, signer, hash) in [
        ("run-good", &arranged.coordinator, workflow_hash(WORKFLOW)),
        ("run-bad", &tamperer, workflow_hash("tampered")),
    ] {
        let spec = CiRunSpec::new(
            &arranged.published,
            run_id,
            head.clone(),
            CiTrigger::push("refs/heads/main"),
            arranged.now,
        )
        .workflow("ci.yml", hash);
        arranged
            .harness
            .publish_ci_run(&arranged.ci_relay, signer, &spec)
            .await?;
    }

    // No target: HEAD.
    let (out, json) = ci_status(&arranged.publisher, &[]).await?;
    assert!(
        out.status.success(),
        "`ngit ci status` exited {:?}",
        out.status
    );
    assert_eq!(json["ci"]["state"], "concluded");
    assert_eq!(json["ci"]["conclusion"], "success");
    assert_eq!(json["ci"]["revision_matched"], true);
    assert_eq!(json["target"]["kind"], "commit");
    assert_eq!(json["target"]["commit"], head);
    assert_eq!(runs(&json).len(), 2, "both workflows are current: {json}");

    let coordinator_npub = arranged.coordinator.public_key().to_bech32()?;
    let good = runs(&json)
        .iter()
        .find(|run| run["coordinator"] == coordinator_npub.as_str())
        .with_context(|| format!("no run from the coordinator in {json}"))?;
    assert_eq!(good["workflow"], "ci.yml");
    assert_eq!(good["integrity"]["commit_present"], true);
    assert_eq!(good["integrity"]["workflow_hash_matches"], true);
    assert_eq!(good["classification"], "no-known-context");
    assert_eq!(good["attempt_of"], 1);

    let tampered_npub = tamperer.public_key().to_bech32()?;
    let bad = runs(&json)
        .iter()
        .find(|run| run["coordinator"] == tampered_npub.as_str())
        .with_context(|| format!("no run from the second coordinator in {json}"))?;
    assert_eq!(bad["integrity"]["commit_present"], true);
    assert_eq!(
        bad["integrity"]["workflow_hash_matches"], false,
        "a workflow file that does not hash to the signed value is a mismatch"
    );

    // An explicit commit-ish resolves the same way as the HEAD default.
    let (out, by_oid) = ci_status(&arranged.publisher, &[head.as_str()]).await?;
    assert!(out.status.success());
    assert_eq!(by_oid["target"]["commit"], head);
    assert_eq!(runs(&by_oid).len(), 2);

    // An annotated tag is peeled, and both spellings are queried.
    arranged
        .publisher
        .git_ok(
            ["tag", "-a", "v1", "-m", "release 1", "HEAD"],
            "git tag -a v1",
        )
        .await?;
    let tag_oid = arranged.publisher.rev_parse("v1").await?;
    assert_ne!(tag_oid, head, "an annotated tag has its own object id");

    // A tag-triggered run carries both `c` values, the peeled commit first.
    let dual = CiRunSpec::new(
        &arranged.published,
        "run-tagged",
        head.clone(),
        CiTrigger::push("refs/tags/v1"),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .also_commit(tag_oid.clone());
    let tag_coordinator = Keys::generate();
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &tag_coordinator, &dual)
        .await?;

    for target in [
        "v1",
        tag_oid.as_str(),
        // `#c` matching is byte-for-byte, so an uppercase commit-ish must
        // still find lowercase tag values.
        head.to_uppercase().as_str(),
    ] {
        let (out, by_tag) = ci_status(&arranged.publisher, &[target]).await?;
        assert!(out.status.success(), "for target {target}");
        assert_eq!(
            runs(&by_tag).len(),
            3,
            "peeling found every run for the commit, including the dual-`c` \
             tag run: {by_tag}"
        );
        let tagged = runs(&by_tag)
            .iter()
            .find(|run| run["coordinator"] == tag_coordinator.public_key().to_bech32().unwrap())
            .with_context(|| format!("no tag-triggered run for target {target}: {by_tag}"))?;
        assert_eq!(tagged["integrity"]["commit_present"], true);
        assert_eq!(tagged["integrity"]["workflow_hash_matches"], true);
    }

    let (_, by_tag) = ci_status(&arranged.publisher, &["v1"]).await?;
    assert_eq!(
        by_tag["target"]["queried"],
        serde_json::json!([head, tag_oid]),
        "an annotated tag is queried by the peeled commit and the tag object"
    );

    // A commit with no CI at all is `none`, and still matched.
    arranged
        .publisher
        .git_ok(["commit", "--allow-empty", "-m", "no ci"], "git commit")
        .await?;
    let (out, empty) = ci_status(&arranged.publisher, &[]).await?;
    assert!(out.status.success());
    assert_eq!(empty["ci"]["state"], "none");
    assert_eq!(empty["ci"]["conclusion"], Value::Null);
    assert_eq!(empty["ci"]["revision_matched"], true);
    assert!(runs(&empty).is_empty());

    Ok(())
}

#[tokio::test]
async fn pr_targets_resolve_by_prefix_nevent_full_hex_and_bare_short_hex() -> Result<()> {
    let arranged = arrange("ci-pr").await?;
    let pr: PublishedPr = arranged
        .harness
        .publish_pr(
            &arranged.published,
            PublishPrOpts {
                branch: None,
                commits: Vec::new(),
                title: "a pr with ci".to_string(),
                description: "body".to_string(),
                in_reply_to: Vec::new(),
            },
        )
        .await?;

    // The coordinator ran `build` itself and accepted it; a separate provider
    // claims `test` without the coordinator quoting it.
    let provider = Keys::generate();
    let spec = CiRunSpec::new(
        &arranged.published,
        "run-pr",
        pr.tip.clone(),
        CiTrigger::pull_request(&pr),
        arranged.now,
    )
    .conclusion(Some("failure"))
    .job(CiJob::new("build", "failure", &arranged.coordinator))
    .job(CiJob::new("test", "success", &provider).unaccepted());
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &spec)
        .await?;

    let pr_hex = pr.event_id.to_hex();
    let pr_nevent = nevent(pr.event_id)?;
    let prefix = format!("#{}", &pr_hex[..8]);
    // A bare short hex is tried as a commit-ish first and falls back to a PR
    // event-id prefix.
    let bare_prefix = pr_hex[..8].to_string();

    for target in [
        prefix.as_str(),
        pr_nevent.as_str(),
        pr_hex.as_str(),
        bare_prefix.as_str(),
    ] {
        let (out, json) = ci_status(&arranged.publisher, &[target]).await?;
        assert!(
            out.status.success(),
            "`ngit ci status {target}` exited {:?}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(json["target"]["kind"], "pr", "for target {target}");
        assert_eq!(
            event_id_of(&json["target"]["pr"]),
            pr.event_id,
            "for target {target}"
        );
        assert_eq!(
            event_id_of(&json["target"]["anchor"]),
            pr.event_id,
            "a thread that started as a PR anchors on itself"
        );
        assert_eq!(json["ci"]["state"], "concluded", "for target {target}");
        assert_eq!(json["ci"]["conclusion"], "failure", "for target {target}");
        assert_eq!(json["ci"]["revision_matched"], true);
        let run = &runs(&json)[0];
        let jobs = run["jobs"].as_array().expect("jobs array");
        assert_eq!(jobs.len(), 2, "both job claims are surfaced: {json}");
        let unaccepted = jobs
            .iter()
            .find(|job| job["job"] == "test")
            .expect("the unaccepted job claim");
        assert_eq!(unaccepted["provider"], provider.public_key().to_bech32()?);
        assert_eq!(
            unaccepted["classification"], "no-known-context",
            "coordinator trust reaches a provider only through an accepting quote"
        );
        assert_eq!(
            run["integrity"]["commit_present"], false,
            "the PR's commits were never fetched into this repository, so \
             integrity is reported as unheld rather than as an error"
        );
        assert_eq!(run["integrity"]["workflow_hash_matches"], Value::Null);
    }

    // A bare short hex that is neither a commit-ish nor a PR prefix must say
    // how to force the event-prefix interpretation.
    let (out, json) = ci_status(&arranged.publisher, &["abcdef"]).await?;
    assert!(!out.status.success(), "an unresolvable target must fail");
    assert_eq!(json["command_status"], "error");
    let error = json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("#abcdef"),
        "the ambiguity error must suggest the event-prefix form: {error}"
    );

    Ok(())
}

#[tokio::test]
async fn pr_ci_is_reported_for_the_latest_revision_only() -> Result<()> {
    let arranged = arrange("ci-revision").await?;
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
        .git_ok(["checkout", "-b", "revised"], "git checkout -b revised")
        .await?;
    let first_tip = commit_file(&contributor, "one.md", "one\n").await?;
    ngit_ok(&contributor, &["send", "--defaults", "--force-pr"]).await?;
    let root = find_by_branch(&arranged.harness, KIND_PULL_REQUEST, "revised").await?;

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
        branch_name: "revised".to_string(),
        commits: vec![first_tip.clone()],
        tip: first_tip.clone(),
        root_event: root.clone(),
    };

    // A result for the superseded revision is not the PR's current state.
    let outdated = CiRunSpec::new(
        &arranged.published,
        "run-outdated",
        first_tip,
        CiTrigger::pull_request(&pr),
        arranged.now,
    )
    .conclusion(Some("success"));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &outdated)
        .await?;

    let (out, json) = ci_status(&arranged.publisher, &[&root.id.to_hex()]).await?;
    assert!(out.status.success());
    assert_eq!(event_id_of(&json["target"]["revision"]), revision.id);
    assert_eq!(
        json["ci"]["state"], "none",
        "a result for an earlier revision is never presented as current: {json}"
    );
    assert_eq!(
        json["ci"]["revision_matched"], false,
        "CI exists for this PR, but not for its current revision: {json}"
    );
    assert!(runs(&json).is_empty());

    // The current revision's own result is reported.
    let current = CiRunSpec::new(
        &arranged.published,
        "run-current",
        second_tip,
        CiTrigger::pull_request_revision(&pr, &revision),
        arranged.now,
    )
    .conclusion(Some("failure"));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &current)
        .await?;

    let (out, json) = ci_status(&arranged.publisher, &[&root.id.to_hex()]).await?;
    assert!(out.status.success());
    assert_eq!(json["ci"]["state"], "concluded");
    assert_eq!(json["ci"]["conclusion"], "failure");
    assert_eq!(json["ci"]["revision_matched"], true);
    assert_eq!(runs(&json).len(), 1, "only the current revision's run");

    Ok(())
}

#[tokio::test]
async fn ci_anchors_on_the_pr_upgrade_root_of_an_upgraded_patch_thread() -> Result<()> {
    let arranged = arrange("ci-upgrade").await?;
    let contributor = arranged
        .harness
        .clone_published_repo(
            &arranged.published,
            CloneLogin::AsContributor {
                display_name: "upgrade contributor".to_string(),
            },
        )
        .await?;

    contributor
        .git_ok(["checkout", "-b", "upgraded"], "git checkout -b upgraded")
        .await?;
    commit_file(&contributor, "patched.md", "patched\n").await?;
    ngit_ok(&contributor, &["send", "--defaults", "--force-patch"]).await?;
    let patch_root = find_by_branch(&arranged.harness, Kind::GitPatch, "upgraded").await?;

    // The maintainer upgrades the patch thread to a PR. The upgrade root is a
    // kind-1618 event that back-references the patch root, and it is what CI
    // events anchor on.
    ngit_ok(
        &arranged.publisher,
        &["pr", "checkout", &patch_root.id.to_hex()],
    )
    .await?;
    ngit_ok(
        &arranged.publisher,
        &[
            "send",
            "--defaults",
            "--force-pr",
            "--in-reply-to",
            &patch_root.id.to_hex(),
        ],
    )
    .await?;
    let upgrade_root = find_by_branch(&arranged.harness, KIND_PULL_REQUEST, "upgraded").await?;
    assert_ne!(upgrade_root.id, patch_root.id);

    let tip = arranged.publisher.rev_parse("HEAD").await?;
    let spec = CiRunSpec::new(
        &arranged.published,
        "run-upgraded",
        tip,
        CiTrigger::PullRequest {
            root: upgrade_root.id,
            root_author: upgrade_root.pubkey,
            supplying: upgrade_root.id,
            supplying_kind: 1618,
            supplying_author: upgrade_root.pubkey,
        },
        arranged.now,
    )
    .conclusion(Some("success"));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &spec)
        .await?;

    // The thread is still identified by the patch root, from either spelling,
    // and both resolve to the upgrade root as the CI anchor.
    let patch_prefix = format!("#{}", &patch_root.id.to_hex()[..8]);
    for target in [patch_prefix.as_str(), &upgrade_root.id.to_hex()] {
        let (out, json) = ci_status(&arranged.publisher, &[target]).await?;
        assert!(
            out.status.success(),
            "`ngit ci status {target}` exited {:?}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(
            event_id_of(&json["target"]["pr"]),
            patch_root.id,
            "the thread keeps the patch root as its identity: {json}"
        );
        assert_eq!(
            event_id_of(&json["target"]["anchor"]),
            upgrade_root.id,
            "CI anchors on the upgrade root: {json}"
        );
        assert_eq!(
            json["ci"]["state"], "concluded",
            "the upgraded thread's CI is found: {json}"
        );
        assert_eq!(json["ci"]["conclusion"], "success");
        assert_eq!(runs(&json).len(), 1);
    }

    Ok(())
}

#[tokio::test]
async fn run_state_follows_progress_markers_and_the_worst_conclusion() -> Result<()> {
    let arranged = arrange("ci-state").await?;
    let head = arranged.publisher.rev_parse("HEAD").await?;
    let hash = workflow_hash(WORKFLOW);

    // An unexpired queued/in-progress marker with no result is `running`.
    let running = CiRunSpec::new(
        &arranged.published,
        "run-running",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", hash.clone())
    .conclusion(None)
    .progress(CiProgress::running(arranged.now));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &running)
        .await?;

    let (out, json) = ci_status(&arranged.publisher, &[]).await?;
    assert!(out.status.success());
    assert_eq!(json["ci"]["state"], "running");
    assert_eq!(json["ci"]["conclusion"], Value::Null);

    // A second workflow concludes `failure`; the rollup takes the worst of
    // the concluded runs while the first is still running.
    let failed = CiRunSpec::new(
        &arranged.published,
        "run-failed",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("release.yml", hash.clone())
    .conclusion(Some("failure"));
    let succeeded = CiRunSpec::new(
        &arranged.published,
        "run-succeeded",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("docs.yml", hash.clone())
    .conclusion(Some("success"));
    for spec in [&failed, &succeeded] {
        arranged
            .harness
            .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, spec)
            .await?;
    }

    let (out, json) = ci_status(&arranged.publisher, &[]).await?;
    assert!(out.status.success());
    assert_eq!(
        json["ci"]["state"], "running",
        "a live marker keeps the target running: {json}"
    );

    // Conclude the running workflow with `success`: the rollup is still the
    // worst of the three.
    let concluded = CiRunSpec::new(
        &arranged.published,
        "run-running",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", hash.clone())
    .conclusion(Some("success"));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &concluded)
        .await?;

    let (out, json) = ci_status(&arranged.publisher, &[]).await?;
    assert!(out.status.success());
    assert_eq!(json["ci"]["state"], "concluded");
    assert_eq!(
        json["ci"]["conclusion"], "failure",
        "failure beats success in the worst-of rollup: {json}"
    );

    Ok(())
}

#[tokio::test]
async fn an_expired_progress_marker_with_no_result_is_stale() -> Result<()> {
    let arranged = arrange("ci-stale").await?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    let stale = CiRunSpec::new(
        &arranged.published,
        "run-stale",
        head,
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW))
    .conclusion(None)
    .progress(CiProgress::expired(arranged.now));
    let events = build_ci_run(&arranged.coordinator, &stale)?;

    // A relay refuses an already-expired event (NIP-40), and waiting for a
    // live marker to expire would be a wall-clock sleep. The state this
    // exercises is a marker ngit fetched while it was live and still holds:
    // seed the local cache the fetch would have written to.
    for event in events.all() {
        ngit::client::save_event_in_local_cache(arranged.publisher.dir(), &event)
            .await
            .context("failed to seed the expired progress marker into the local cache")?;
    }

    let (out, json) = ci_status(&arranged.publisher, &[]).await?;
    assert!(out.status.success());
    assert_eq!(
        json["ci"]["state"], "stale",
        "an expired marker with no result is abandoned, not running: {json}"
    );
    assert_eq!(json["ci"]["conclusion"], Value::Null);

    // A target that has not concluded cannot meet any trust floor.
    let (out, gated) = ci_status(
        &arranged.publisher,
        &["--require-ci-trust", "operationally-associated"],
    )
    .await?;
    assert!(!out.status.success(), "a stale target must fail the gate");
    assert_eq!(gated["command_status"], "error");
    assert!(
        gated["error"]
            .as_str()
            .is_some_and(|error| error.contains("stale")),
        "the refusal names the state: {gated}"
    );

    Ok(())
}

#[tokio::test]
async fn require_ci_trust_gates_on_the_weakest_current_run() -> Result<()> {
    let arranged = arrange("ci-trust").await?;
    let head = arranged.publisher.rev_parse("HEAD").await?;
    let hash = workflow_hash(WORKFLOW);
    let coordinate = repo_coordinate(&arranged.published);

    // A confirmed maintainer's standing Service Request, signed before the
    // run started, covers it.
    let request = build_service_control(
        &arranged.published.maintainer_keys,
        &arranged.coordinator.public_key(),
        &coordinate,
        true,
        arranged.now - 600,
    )?;
    arranged
        .harness
        .publish_ci_events(&arranged.ci_relay, std::slice::from_ref(&request))
        .await?;

    let covered = CiRunSpec::new(
        &arranged.published,
        "run-covered",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", hash.clone())
    .started_at(arranged.now - 300)
    .provenance(CiProvenance::service_request(&request));
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &covered)
        .await?;

    let (out, json) = ci_status(
        &arranged.publisher,
        &["--require-ci-trust", "operationally-associated"],
    )
    .await?;
    assert!(
        out.status.success(),
        "a maintainer-directed success meets the floor: {json}"
    );
    assert_eq!(json["command_status"], "ok");
    assert_eq!(runs(&json)[0]["classification"], "maintainer-directed");
    assert!(
        runs(&json)[0]["evidence"]
            .as_array()
            .is_some_and(|evidence| !evidence.is_empty()),
        "maintainer direction is reported as evidence: {json}"
    );

    // The same run meets the strictest floor.
    let (out, _) = ci_status(
        &arranged.publisher,
        &["--require-ci-trust", "maintainer-directed"],
    )
    .await?;
    assert!(out.status.success());

    // The offline tier reads the same cache but skips the domain ladder, so
    // its coverage is partial.
    let (out, offline) = ci_status(&arranged.publisher, &["--offline"]).await?;
    assert!(out.status.success());
    assert_eq!(offline["ci"]["coverage"], "partial");
    assert_eq!(runs(&offline).len(), 1, "the cache tier still has the run");
    assert_eq!(runs(&offline)[0]["classification"], "maintainer-directed");

    // A second coordinator nobody asked for: the rollup surfaces the weakest
    // run, so the gate refuses even though both runs succeeded.
    let stranger = Keys::generate();
    let uncovered = CiRunSpec::new(
        &arranged.published,
        "run-uncovered",
        head.clone(),
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", hash.clone());
    arranged
        .harness
        .publish_ci_run(&arranged.ci_relay, &stranger, &uncovered)
        .await?;

    for floor in ["operationally-associated", "maintainer-directed"] {
        let (out, json) = ci_status(&arranged.publisher, &["--require-ci-trust", floor]).await?;
        assert!(
            !out.status.success(),
            "a run with no known context must fail the {floor} floor: {json}"
        );
        assert_eq!(json["command_status"], "error");
        assert_eq!(
            runs(&json).len(),
            2,
            "the refusal keeps the runs that explain it: {json}"
        );
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|error| error.contains("No known context")),
            "the refusal names the weakest classification: {json}"
        );
    }

    // Without the flag the same state is reported, and the command succeeds.
    let (out, _) = ci_status(&arranged.publisher, &[]).await?;
    assert!(out.status.success());

    Ok(())
}

#[tokio::test]
async fn a_neutral_or_skipped_conclusion_passes_the_gate_and_a_cancelled_one_does_not() -> Result<()>
{
    let arranged = arrange("ci-green").await?;
    let hash = workflow_hash(WORKFLOW);

    // A confirmed maintainer's standing request covers every run below, so
    // the only thing the gate can be reacting to is the conclusion itself.
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

    // One commit per conclusion, so no run is ever rolled up with another.
    for (file, run_id, conclusion, green) in [
        ("neutral.md", "run-neutral", "neutral", true),
        ("skipped.md", "run-skipped", "skipped", true),
        // The control: `cancelled` is a concluded run too, and nothing ran to
        // completion, so it is refused at the same trust.
        ("cancelled.md", "run-cancelled", "cancelled", false),
    ] {
        let commit = commit_file(&arranged.publisher, file, conclusion).await?;
        let spec = CiRunSpec::new(
            &arranged.published,
            run_id,
            commit.clone(),
            CiTrigger::push("refs/heads/main"),
            arranged.now,
        )
        .workflow("ci.yml", hash.clone())
        .started_at(arranged.now - 300)
        .conclusion(Some(conclusion))
        .provenance(CiProvenance::service_request(&request));
        arranged
            .harness
            .publish_ci_run(&arranged.ci_relay, &arranged.coordinator, &spec)
            .await?;

        let (out, json) = ci_status(
            &arranged.publisher,
            &[
                commit.as_str(),
                "--require-ci-trust",
                "operationally-associated",
            ],
        )
        .await?;
        assert_eq!(json["ci"]["state"], "concluded", "{json}");
        assert_eq!(json["ci"]["conclusion"], conclusion, "{json}");
        assert_eq!(
            runs(&json)[0]["classification"],
            "maintainer-directed",
            "the gate's verdict below is about the conclusion, not the \
             trust: {json}"
        );
        assert_eq!(
            out.status.success(),
            green,
            "`{conclusion}` must gate as the shared green predicate says: {json}"
        );
        assert_eq!(
            json["command_status"],
            if green { "ok" } else { "error" },
            "{json}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn a_validated_manual_trigger_is_maintainer_direction_for_its_run() -> Result<()> {
    let arranged = arrange("ci-manual").await?;
    let head = arranged.publisher.rev_parse("HEAD").await?;

    let spec = CiRunSpec::new(
        &arranged.published,
        "run-manual",
        head,
        CiTrigger::push("refs/heads/main"),
        arranged.now,
    )
    .workflow("ci.yml", workflow_hash(WORKFLOW));

    // The trigger authorizes exactly this run, and is signed by the
    // repository's confirmed maintainer.
    let trigger = build_manual_trigger(
        &arranged.published.maintainer_keys,
        &arranged.coordinator.public_key(),
        &spec,
        arranged.now - 120,
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
            &spec.provenance(CiProvenance::manual_trigger(&trigger)),
        )
        .await?;

    let (out, json) = ci_status(
        &arranged.publisher,
        &["--require-ci-trust", "maintainer-directed"],
    )
    .await?;
    assert!(
        out.status.success(),
        "a validated manual trigger meets the strictest floor: {json}"
    );
    assert_eq!(runs(&json)[0]["classification"], "maintainer-directed");
    let evidence = runs(&json)[0]["evidence"]
        .as_array()
        .expect("evidence array");
    assert!(
        evidence
            .iter()
            .any(|item| item["kind"] == "maintainer-request" && item["scope"] == "run"),
        "the trigger is run-scoped evidence: {json}"
    );
    assert!(
        evidence.iter().any(|item| {
            item["authors"].as_array().is_some_and(|authors| {
                authors.iter().any(|author| {
                    author.as_str()
                        == arranged
                            .published
                            .maintainer_keys
                            .public_key()
                            .to_bech32()
                            .ok()
                            .as_deref()
                })
            })
        }),
        "the requesting maintainer is named as an npub: {json}"
    );

    Ok(())
}
