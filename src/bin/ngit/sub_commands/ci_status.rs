//! `ngit ci status` — CI results for a PR, a commit-ish, or HEAD, with the
//! trust context of every signer behind them.
//!
//! The CI events themselves arrive through the repository fetch every PR
//! command performs (`client::get_filter_ci_events`); this command reads them
//! from the local cache, groups them into runs, and labels each run with the
//! trust model in `ngit::ci`.
//!
//! Two things are deliberately kept apart in the output:
//!
//! - **Trust context** — why a result may deserve attention. Absence of
//!   evidence is "No known context", never "untrusted".
//! - **Integrity** — whether ngit's own git objects agree with what the
//!   coordinator signed. It is not a trust level, and a commit ngit does not
//!   hold makes it unknown rather than false.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bitcoin_hashes::sha256;
use ngit::{
    ci::{
        domain::{NetworkNip05Lookup, Nip05Cache},
        events::{RunState, WorkflowRun, group_workflow_runs, latest_attempts},
        kinds::{self, CONSUMED_CI_KINDS, Conclusion, JobResult, ServiceControl},
        provenance::{WantedQuote, wanted_quotes},
        resolve::{
            CiInputs, CiTrustContext, QuotedEventFetcher, RepositoryContext, resolve_cache_tier,
            resolve_full_tier,
        },
        trust::{CONTEXT_INCOMPLETE_LABEL, Coverage, TrustClassification, TrustResolution},
    },
    client::{
        Client, Connect, Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_events_from_local_cache, get_proposals_and_revisions_from_cache,
        get_repo_ref_from_cache,
    },
    event_ordering::latest_event,
    git::{Repo, RepoActions},
    git_events::{KIND_PULL_REQUEST_UPDATE, event_is_revision_root},
    repo_ref::RepoRef,
    utils::pr_upgrade_root,
};
use nostr::prelude::{
    Event, EventId, Filter, Kind, Metadata, PublicKey, RelayUrl, SingleLetterTag, Timestamp,
    ToBech32,
};
use serde_json::{Value, json};

use crate::{
    cli::{CiTrustFloor, SignerParams},
    repo_ref::get_repo_coordinates_when_remote_unknown,
    sub_commands::{
        id_resolver::{parse_event_id, pr_description, resolve_pr_root_or_prefix},
        repository_fetch::fetching_with_account,
    },
};

/// Run `ngit ci status`.
///
/// # Errors
///
/// Returns an error when the repository, the target, or the local cache
/// cannot be read. A failed `--require-ci-trust` check is not an error: the
/// document is emitted in full and the process exits non-zero.
pub async fn launch(
    target: Option<&str>,
    offline: bool,
    require_ci_trust: Option<CiTrustFloor>,
    json: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let mut repo_coordinates =
        get_repo_coordinates_when_remote_unknown(&git_repo, &mut client).await?;

    let fetch_report = if offline {
        None
    } else {
        Some(
            fetching_with_account(
                &git_repo,
                git_repo_path,
                &mut client,
                &mut repo_coordinates,
                auth,
            )
            .await?,
        )
    };

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    // A repository relay that did not answer leaves this view incomplete
    // whatever the trust layer finds locally.
    let input_coverage = fetch_report.as_ref().map_or(Coverage::Complete, |report| {
        relay_coverage(&repo_ref.relays, &report.state_per_relay)
    });
    let target = resolve_target(&git_repo, git_repo_path, &repo_ref, target).await?;
    let report = build_report(
        &git_repo,
        git_repo_path,
        &repo_ref,
        &client,
        &target,
        offline,
        input_coverage,
    )
    .await?;

    let gate = require_ci_trust.and_then(|floor| report.gate_failure(floor));

    if json {
        crate::output::set_value(report.to_json(&target, repo_ref.relays.first(), gate.as_deref()));
    }
    report.print(&target);

    if let Some(reason) = gate {
        println!("{reason}");
        // The document is this command's real output, so it is emitted before
        // exiting: `main`'s error path would replace it with an error document
        // and lose the runs that explain the refusal.
        crate::output::finish_and_exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// What the command was asked about, after resolution.
enum Target {
    /// A cached PR thread, the event its CI anchors on, and the revision that
    /// is current for it.
    PullRequest {
        /// The thread root ngit identifies the PR by.
        root: EventId,
        /// The kind-1618 event CI events name in `E`. For a thread that
        /// started as patches and was later upgraded, that is the
        /// PR-upgrade root, not the patch root ngit threads from.
        anchor: EventId,
        /// The anchor itself, or the newest kind-1619 update.
        revision: EventId,
    },
    /// A commit-ish. `ids` holds the peeled commit and, for an annotated tag,
    /// the tag object id as well, so one `#c` query finds either spelling.
    Commit { ids: Vec<String>, described: String },
}

/// Resolve `<target>` in the documented order.
///
/// 1. `#<hex-prefix>` is always an event prefix.
/// 2. An nevent/note or a full 64-character hex id is always an event id, and
///    must name a cached PR root or one of its revisions.
/// 3. Otherwise a commit-ish, resolved by git.
/// 4. A bare short hex that is not a commit-ish falls back to a PR prefix.
/// 5. No target is HEAD.
async fn resolve_target(
    git_repo: &Repo,
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    target: Option<&str>,
) -> Result<Target> {
    let Some(raw) = target.map(str::trim).filter(|raw| !raw.is_empty()) else {
        let head = git_repo
            .get_head_commit()
            .context("failed to resolve HEAD; is this an empty repository?")?;
        return Ok(Target::Commit {
            ids: vec![head.to_string()],
            described: "HEAD".to_owned(),
        });
    };

    let proposals =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    if raw.starts_with('#') {
        let root = resolve_pr_root_or_prefix(raw, proposals.iter(), pr_description)?;
        return pull_request_target(git_repo_path, repo_ref, root).await;
    }

    if let Ok(event_id) = parse_event_id(raw) {
        let root = pr_root_for_event(git_repo_path, &proposals, event_id).await?;
        return pull_request_target(git_repo_path, repo_ref, &root).await;
    }

    if let Some(commit) = commit_ish_target(git_repo, raw) {
        return Ok(commit);
    }

    if is_hex_prefix(raw) {
        let root = resolve_pr_root_or_prefix(raw, proposals.iter(), pr_description).with_context(
            || {
                format!(
                    "`{raw}` is not a commit-ish in this repository, so it was resolved as a PR event-id prefix; \
                     use `#{raw}` to skip the commit-ish attempt"
                )
            },
        )?;
        return pull_request_target(git_repo_path, repo_ref, root).await;
    }

    bail!(
        "`{raw}` is neither a commit-ish nor an event id; use `#<hex-prefix>`, an nevent, or a full event id for a PR"
    );
}

/// Build a PR target: the event CI anchors on, and the revision that is
/// current for it.
///
/// A thread that started as patches and was upgraded to a PR keeps the patch
/// root as its ngit identity, while every later PR update — and every CI
/// event — references the kind-1618 upgrade root. `utils::pr_upgrade_root`
/// is the one rule that decides which event that is.
async fn pull_request_target(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    root: &Event,
) -> Result<Target> {
    let thread =
        get_all_proposal_patch_pr_pr_update_events_from_cache(git_repo_path, repo_ref, &root.id)
            .await?;
    let anchor = pr_upgrade_root(&thread).map_or(root.id, |event| event.id);
    let revision = latest_event(
        thread
            .iter()
            .filter(|event| event.kind == KIND_PULL_REQUEST_UPDATE),
    )
    .map_or(anchor, |event| event.id);
    Ok(Target::PullRequest {
        root: root.id,
        anchor,
        revision,
    })
}

/// Map an exact event id onto the PR thread root it belongs to.
///
/// A revision root — a patch revision, or the kind-1618 root of an upgraded
/// patch thread — resolves to the proposal it revises, as does a kind-1619
/// update, so `ngit ci status <nevent-of-a-revision>` describes that PR
/// rather than failing.
async fn pr_root_for_event(
    git_repo_path: &Path,
    proposals: &[Event],
    event_id: EventId,
) -> Result<Event> {
    if let Some(root) = proposals
        .iter()
        .find(|event| event.id == event_id && !event_is_revision_root(event))
    {
        return Ok(root.clone());
    }

    let cached = get_events_from_local_cache(git_repo_path, vec![Filter::default().id(event_id)])
        .await?
        .into_iter()
        .next()
        .with_context(|| format!("no cached PR or revision with id {event_id}"))?;

    let root_id = root_reference(&cached).with_context(|| {
        format!("cached event {event_id} is not a PR root and names no PR root")
    })?;
    proposals
        .iter()
        .find(|event| event.id == root_id)
        .cloned()
        .with_context(|| format!("PR {root_id}, revised by {event_id}, is not in the cache"))
}

/// The proposal a revision names, in decreasing order of confidence: the
/// NIP-22 `E` tag of a kind-1619 update, a NIP-10 `e` tag marked `root` on a
/// patch revision, and finally an unmarked `e` — which is the shape a
/// patch-to-PR upgrade root uses to back-reference the patch it upgrades
/// (`git_events::generate_unsigned_pr_or_update_event`).
fn root_reference(event: &Event) -> Option<EventId> {
    let referenced = |name: &'static str, marked_root: bool| {
        event
            .tags
            .iter()
            .filter(move |tag| {
                tag.as_slice().first().is_some_and(|first| first == name)
                    && (!marked_root
                        || tag.as_slice().get(3).is_some_and(|marker| marker == "root"))
            })
            .filter_map(|tag| tag.as_slice().get(1))
            .find_map(|value| EventId::parse(value).ok())
    };
    referenced("E", false)
        .or_else(|| referenced("e", true))
        .or_else(|| referenced("e", false))
}

/// Resolve `raw` as a commit-ish, peeling an annotated tag to its commit
/// while keeping the tag object id as a second `#c` candidate.
fn commit_ish_target(git_repo: &Repo, raw: &str) -> Option<Target> {
    let object = git_repo.git_repo.revparse_single(raw).ok()?;
    let commit = object.peel_to_commit().ok()?;
    let mut ids = vec![commit.id().to_string()];
    let object_id = object.id().to_string();
    if object_id != ids[0] {
        ids.push(object_id);
    }
    Some(Target::Commit {
        described: format!("{raw} ({})", short(&ids[0])),
        ids,
    })
}

fn is_hex_prefix(value: &str) -> bool {
    !value.is_empty() && value.len() < 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

impl Target {
    /// The cache query for this target's CI events.
    fn ci_filter(&self) -> Filter {
        let filter = Filter::default().kinds(CONSUMED_CI_KINDS.to_vec());
        match self {
            Self::PullRequest { anchor, .. } => {
                filter.custom_tags(SingleLetterTag::UPPERCASE_E, [*anchor])
            }
            // Git object ids are case-insensitive hex; tag values are matched
            // byte for byte, and the NIP has publishers emit lowercase.
            Self::Commit { ids, .. } => filter.custom_tags(
                SingleLetterTag::LOWERCASE_C,
                ids.iter()
                    .map(|id| id.to_lowercase())
                    .collect::<Vec<String>>(),
            ),
        }
    }

    /// The runs this target presents as current, and whether any run the
    /// target has CI for describes it.
    ///
    /// A PR presents only the runs for its latest revision: an attempt for an
    /// earlier revision is never current, even when it is newer.
    fn select(&self, runs: &[WorkflowRun]) -> (Vec<WorkflowRun>, bool) {
        let selected: Vec<WorkflowRun> = match self {
            Self::PullRequest {
                anchor, revision, ..
            } => runs
                .iter()
                .filter(|run| {
                    run.pr_root == Some(*anchor)
                        && run
                            .supplying_event
                            .map_or(*revision == *anchor, |supplying| supplying == *revision)
                })
                .cloned()
                .collect(),
            Self::Commit { ids, .. } => runs
                .iter()
                .filter(|run| {
                    run.commits
                        .iter()
                        .any(|commit| ids.iter().any(|id| id.eq_ignore_ascii_case(commit)))
                })
                .cloned()
                .collect(),
        };
        // Nothing at all is "no CI", not "CI for something else".
        let matched = !selected.is_empty() || runs.is_empty();
        (selected, matched)
    }

    fn describe(&self) -> String {
        match self {
            Self::PullRequest {
                root,
                anchor,
                revision,
            } => {
                let id = short(&root.to_hex());
                if revision == anchor {
                    format!("PR {id}")
                } else {
                    format!("PR {id} (revision {})", short(&revision.to_hex()))
                }
            }
            Self::Commit { described, .. } => described.clone(),
        }
    }

    /// Event ids are bech32 `nevent`s, as on every other ngit JSON surface.
    fn to_json(&self, relay: Option<&RelayUrl>) -> Value {
        match self {
            Self::PullRequest {
                root,
                anchor,
                revision,
            } => json!({
                "kind": "pr",
                "pr": crate::output::event_id_to_nevent(*root, relay),
                // The event CI anchors on, which differs from `pr` only for a
                // patch thread that was upgraded to a PR.
                "anchor": crate::output::event_id_to_nevent(*anchor, relay),
                "revision": crate::output::event_id_to_nevent(*revision, relay),
            }),
            Self::Commit { ids, described } => json!({
                "kind": "commit",
                "commit": ids.first(),
                "commit_ish": described,
                "queried": ids,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Whether the target's current runs are executing, finished, or abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CiState {
    Running,
    Concluded,
    Stale,
    None,
}

impl CiState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Concluded => "concluded",
            Self::Stale => "stale",
            Self::None => "none",
        }
    }
}

/// Whether ngit's own git objects agree with what the coordinator signed.
///
/// Not a trust level: a commit ngit does not hold leaves the workflow hash
/// unknown rather than mismatched.
struct Integrity {
    commit_present: bool,
    workflow_hash_matches: Option<bool>,
}

struct JobReport {
    job_id: String,
    conclusion: Conclusion,
    provider: PublicKey,
    resolution: TrustResolution,
}

struct RunReport {
    run: WorkflowRun,
    state: RunState,
    /// 1-based position of this attempt among the attempts seen for the same
    /// coordinator and workflow on this target.
    attempt_of: usize,
    resolution: TrustResolution,
    integrity: Integrity,
    jobs: Vec<JobReport>,
}

struct CiReport {
    state: CiState,
    conclusion: Option<Conclusion>,
    revision_matched: bool,
    coverage: Coverage,
    /// The conservative rollup across the current runs.
    rollup: TrustResolution,
    runs: Vec<RunReport>,
    /// Events that were not grouped, with the reason.
    skipped: Vec<String>,
}

impl CiReport {
    fn is_incomplete(&self) -> bool {
        self.coverage != Coverage::Complete
    }

    /// Why `--require-ci-trust` refuses, or `None` when the gate passes.
    fn gate_failure(&self, floor: CiTrustFloor) -> Option<String> {
        if self.state != CiState::Concluded {
            return Some(format!(
                "--require-ci-trust={}: CI has not concluded for this target (state: {})",
                floor.as_str(),
                self.state.as_str(),
            ));
        }
        if self.conclusion != Some(Conclusion::Success) {
            return Some(format!(
                "--require-ci-trust={}: CI concluded {}",
                floor.as_str(),
                self.conclusion.map_or("unknown", Conclusion::as_str),
            ));
        }
        // Both tiers always settle, so a classification is always present.
        let classification = self.rollup.classification()?;
        if meets_floor(classification, floor) {
            return None;
        }
        Some(format!(
            "--require-ci-trust={}: the weakest current run is {}",
            floor.as_str(),
            classification.label(),
        ))
    }

    fn to_json(
        &self,
        target: &Target,
        relay: Option<&RelayUrl>,
        gate_failure: Option<&str>,
    ) -> Value {
        let mut document = json!({
            "status": if gate_failure.is_some() { "error" } else { "ok" },
            "entity": "ci",
            "target": target.to_json(relay),
            "ci": {
                "state": self.state.as_str(),
                "conclusion": self.conclusion.map(Conclusion::as_str),
                "revision_matched": self.revision_matched,
                "coverage": self.coverage.as_str(),
                "runs": self.runs.iter().map(RunReport::to_json).collect::<Vec<Value>>(),
            },
        });
        if !self.skipped.is_empty() {
            document["ci"]["skipped"] = json!(self.skipped);
        }
        if let Some(reason) = gate_failure {
            document["error"] = json!(reason);
        }
        document
    }

    fn print(&self, target: &Target) {
        println!("CI for {}", target.describe());
        if self.runs.is_empty() {
            if self.revision_matched {
                println!("  no CI results");
            } else {
                println!("  no CI results for the current revision");
            }
        }
        for run in &self.runs {
            run.print();
        }
        if let Some(conclusion) = self.conclusion {
            println!("  {} ({conclusion})", self.state.as_str());
        } else if !self.runs.is_empty() {
            println!("  {}", self.state.as_str());
        }
        if self.is_incomplete() && !self.runs.is_empty() {
            println!("  {CONTEXT_INCOMPLETE_LABEL}");
        }
        // Shape rejections are diagnostics for a publisher, not something a
        // reader of this repository can act on, so they follow ngit's
        // verbosity idiom. They stay in the JSON document unconditionally.
        if ngit::client::is_verbose() {
            for skipped in &self.skipped {
                println!("  {skipped}");
            }
        }
    }
}

impl RunReport {
    fn conclusion(&self) -> Option<Conclusion> {
        match self.state {
            RunState::Concluded(conclusion) => Some(conclusion),
            RunState::Running | RunState::Stale => None,
        }
    }

    fn state_label(&self) -> String {
        match self.state {
            RunState::Concluded(conclusion) => conclusion.to_string(),
            RunState::Running => "running".to_owned(),
            RunState::Stale => "stale".to_owned(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "workflow": self.run.workflow_path,
            "state": match self.state {
                RunState::Running => "running",
                RunState::Concluded(_) => "concluded",
                RunState::Stale => "stale",
            },
            "conclusion": self.conclusion().map(Conclusion::as_str),
            "attempt_of": self.attempt_of,
            "run_id": self.run.run_id,
            "commit": self.run.commit(),
            "coordinator": npub(self.run.coordinator),
            "classification": self.resolution.classification().map(TrustClassification::as_str),
            "evidence": self
                .resolution
                .evidence()
                .iter()
                .map(|item| json!({
                    "kind": item.kind.as_str(),
                    "classification": TrustClassification::from(item.classification).as_str(),
                    "summary": item.summary,
                    "authors": item.authors.iter().copied().map(npub).collect::<Vec<String>>(),
                    "scope": item.scope.as_str(),
                }))
                .collect::<Vec<Value>>(),
            "integrity": {
                "commit_present": self.integrity.commit_present,
                "workflow_hash_matches": self.integrity.workflow_hash_matches,
            },
            "jobs": self
                .jobs
                .iter()
                .map(|job| json!({
                    "job": job.job_id,
                    "conclusion": job.conclusion.as_str(),
                    "provider": npub(job.provider),
                    "classification": job.resolution.classification().map(TrustClassification::as_str),
                }))
                .collect::<Vec<Value>>(),
        })
    }

    fn print(&self) {
        let label = self
            .resolution
            .classification()
            .map_or("Checking", TrustClassification::label);
        let evidence = self
            .resolution
            .evidence()
            .first()
            .map_or(String::new(), |item| format!("  {}", item.summary));
        println!(
            "  {:<10} {}  [{label}]{evidence}",
            self.state_label(),
            self.run.workflow_path,
        );
        println!(
            "    integrity: commit {}, workflow hash {}",
            if self.integrity.commit_present {
                "present"
            } else {
                "not held locally"
            },
            match self.integrity.workflow_hash_matches {
                Some(true) => "matches",
                Some(false) => "MISMATCH",
                None => "unknown",
            },
        );
        for job in &self.jobs {
            println!(
                "    job {} {} [{}]",
                job.job_id,
                job.conclusion,
                job.resolution
                    .classification()
                    .map_or("Checking", TrustClassification::label),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn build_report(
    git_repo: &Repo,
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    client: &Client,
    target: &Target,
    offline: bool,
    input_coverage: Coverage,
) -> Result<CiReport> {
    let now = Timestamp::now();
    let repository = RepositoryContext::from_repo_ref(repo_ref);

    let events = get_events_from_local_cache(git_repo_path, vec![target.ci_filter()]).await?;
    let grouped = group_workflow_runs(&events);
    let skipped: Vec<String> = grouped
        .skipped
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let (selected, revision_matched) = target.select(&grouped.runs);
    let attempts = attempt_positions(&selected);
    let current: Vec<WorkflowRun> = latest_attempts(&selected)
        .into_iter()
        .cloned()
        .collect::<Vec<WorkflowRun>>();

    let controls = load_service_controls(git_repo_path, &repository).await?;
    let wanted = wanted_quotes(&current);
    let quoted_events = load_quoted_events(git_repo_path, &wanted).await?;

    let signers = signers(&current, &controls);
    if !offline {
        // The signer-declared identity route needs the signer's kind-0. CI
        // signers are not repository contributors, so the repository fetch
        // never asks for their profiles.
        fetch_signer_profiles(git_repo_path, client, repo_ref, &signers).await;
    }
    let profile_nip05 = load_profile_nip05(git_repo_path, &signers).await?;

    let mut inputs = CiInputs::new(
        &repository,
        &current,
        &controls,
        &quoted_events,
        &profile_nip05,
        now,
    );
    inputs.input_coverage = input_coverage;
    let context = if offline {
        resolve_cache_tier(&inputs)
    } else {
        let fetcher = RelayQuoteFetcher {
            client,
            relays: repo_ref
                .relays
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
        };
        resolve_full_tier(
            &inputs,
            &fetcher,
            &NetworkNip05Lookup,
            Nip05Cache::discover().ok().as_ref(),
        )
        .await
    };

    let runs: Vec<RunReport> = current
        .iter()
        .map(|run| RunReport {
            state: run.state(now),
            attempt_of: attempts
                .get(&(run.coordinator, run.workflow_path.clone()))
                .copied()
                .unwrap_or(1),
            resolution: context.run_resolution(run),
            integrity: check_integrity(git_repo, run),
            jobs: run
                .jobs
                .iter()
                .map(|job| job_report(&context, run, job))
                .collect(),
            run: run.clone(),
        })
        .collect();

    let state = ci_state(&runs);
    Ok(CiReport {
        state,
        conclusion: if state == CiState::Concluded {
            rollup_conclusion(&runs)
        } else {
            None
        },
        revision_matched,
        coverage: context.coverage().unwrap_or(Coverage::Partial),
        rollup: context.summarize(&current),
        runs,
        skipped,
    })
}

fn job_report(context: &CiTrustContext, run: &WorkflowRun, job: &JobResult) -> JobReport {
    JobReport {
        job_id: job.job_id.clone(),
        conclusion: job.conclusion,
        provider: job.author,
        resolution: context.job_resolution(run, job),
    }
}

/// How many attempts each `(coordinator, workflow)` has on this target, which
/// is the position of the current one.
fn attempt_positions(runs: &[WorkflowRun]) -> HashMap<(PublicKey, String), usize> {
    let mut counts: HashMap<(PublicKey, String), usize> = HashMap::new();
    for run in runs {
        *counts
            .entry((run.coordinator, run.workflow_path.clone()))
            .or_insert(0) += 1;
    }
    counts
}

/// The state machine over the current runs.
fn ci_state(runs: &[RunReport]) -> CiState {
    if runs.is_empty() {
        return CiState::None;
    }
    if runs.iter().any(|run| run.state == RunState::Running) {
        return CiState::Running;
    }
    if runs.iter().any(|run| run.conclusion().is_some()) {
        return CiState::Concluded;
    }
    CiState::Stale
}

/// Worst-of rollup: `failure`/`timed_out`/`startup_failure` beat `cancelled`,
/// which beats `success`; `neutral` and `skipped` do not fail the rollup.
fn rollup_conclusion(runs: &[RunReport]) -> Option<Conclusion> {
    runs.iter()
        .filter_map(RunReport::conclusion)
        .max_by_key(|conclusion| conclusion_severity(*conclusion))
}

fn conclusion_severity(conclusion: Conclusion) -> u8 {
    match conclusion {
        Conclusion::Neutral | Conclusion::Skipped => 0,
        Conclusion::Success => 1,
        Conclusion::Cancelled => 2,
        Conclusion::Failure | Conclusion::TimedOut | Conclusion::StartupFailure => 3,
    }
}

fn meets_floor(classification: TrustClassification, floor: CiTrustFloor) -> bool {
    match floor {
        CiTrustFloor::MaintainerDirected => {
            classification == TrustClassification::MaintainerDirected
        }
        CiTrustFloor::OperationallyAssociated => matches!(
            classification,
            TrustClassification::MaintainerDirected | TrustClassification::OperationallyAssociated
        ),
    }
}

/// ngit's local integrity check: does the repository hold the commit, and
/// does the workflow blob at that commit hash to what the coordinator
/// claimed?
fn check_integrity(git_repo: &Repo, run: &WorkflowRun) -> Integrity {
    // Every `c` value is tried and peeled: the NIP puts the commit first, but
    // an annotated-tag run also names the tag object, and a publisher that
    // ordered them the other way round still describes the same commit.
    let Some(commit) = run
        .commits
        .iter()
        .filter_map(|candidate| git2::Oid::from_str(candidate).ok())
        .filter_map(|oid| git_repo.git_repo.find_object(oid, None).ok())
        .find_map(|object| object.peel_to_commit().ok())
    else {
        return Integrity {
            commit_present: false,
            workflow_hash_matches: None,
        };
    };
    let blob_hash = commit
        .tree()
        .ok()
        .and_then(|tree| tree.get_path(Path::new(&run.workflow_path)).ok())
        .and_then(|entry| entry.to_object(&git_repo.git_repo).ok())
        .and_then(|object| object.into_blob().ok())
        .map(|blob| sha256::hash(blob.content()).to_string());
    Integrity {
        commit_present: true,
        workflow_hash_matches: blob_hash.map(|hash| hash.eq_ignore_ascii_case(&run.workflow_hash)),
    }
}

// ---------------------------------------------------------------------------
// Cache reads
// ---------------------------------------------------------------------------

/// Every Service Request/Stop cached for the repository's own coordinates.
async fn load_service_controls(
    git_repo_path: &Path,
    repository: &RepositoryContext,
) -> Result<Vec<ServiceControl>> {
    if repository.coordinates.is_empty() {
        return Ok(Vec::new());
    }
    let events = get_events_from_local_cache(
        git_repo_path,
        vec![
            Filter::default()
                .kinds(vec![
                    kinds::KIND_CI_SERVICE_REQUEST,
                    kinds::KIND_CI_SERVICE_STOP,
                ])
                .custom_tags(
                    SingleLetterTag::LOWERCASE_A,
                    repository
                        .coordinates
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<String>>(),
                ),
        ],
    )
    .await?;
    // A control that breaks the NIP's shape is skipped, never reinterpreted.
    Ok(events
        .iter()
        .filter_map(|event| kinds::validate_service_control(event).ok())
        .collect())
}

/// The quoted requests the local cache already holds.
async fn load_quoted_events(
    git_repo_path: &Path,
    wanted: &[WantedQuote],
) -> Result<HashMap<EventId, Event>> {
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(get_events_from_local_cache(
        git_repo_path,
        vec![Filter::default().ids(wanted.iter().map(|quote| quote.event_id))],
    )
    .await?
    .into_iter()
    .map(|event| (event.id, event))
    .collect())
}

/// Every signer this view describes: run coordinators, job providers, and the
/// coordinators the repository's control history addresses.
fn signers(runs: &[WorkflowRun], controls: &[ServiceControl]) -> HashSet<PublicKey> {
    let mut signers: HashSet<PublicKey> = HashSet::new();
    for run in runs {
        signers.insert(run.coordinator);
        signers.extend(run.jobs.iter().map(|job| job.author));
    }
    signers.extend(controls.iter().map(|control| control.coordinator));
    signers
}

/// Fetch the signers' kind-0 profiles and cache them.
///
/// Best effort by design: a signer with no reachable profile simply has no
/// declared identity to verify, which is an absence of evidence rather than a
/// finding, so a failure here is not surfaced.
async fn fetch_signer_profiles(
    git_repo_path: &Path,
    client: &Client,
    repo_ref: &RepoRef,
    signers: &HashSet<PublicKey>,
) {
    if signers.is_empty() {
        return;
    }
    let mut relays: Vec<String> = repo_ref
        .relays
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    for relay in client.get_relay_default_set() {
        if !relays.contains(relay) {
            relays.push(relay.clone());
        }
    }
    let Ok(events) = client
        .get_events(
            relays,
            vec![
                Filter::default()
                    .kind(Kind::Metadata)
                    .authors(signers.clone()),
            ],
        )
        .await
    else {
        return;
    };
    for event in &events {
        let _ = ngit::client::save_event_in_local_cache(git_repo_path, event).await;
    }
}

/// The `nip05` each signer's cached kind-0 profile claims.
async fn load_profile_nip05(
    git_repo_path: &Path,
    signers: &HashSet<PublicKey>,
) -> Result<HashMap<PublicKey, String>> {
    if signers.is_empty() {
        return Ok(HashMap::new());
    }

    let events = get_events_from_local_cache(
        git_repo_path,
        vec![
            Filter::default()
                .kind(Kind::Metadata)
                .authors(signers.clone()),
        ],
    )
    .await?;

    let mut latest: HashMap<PublicKey, &Event> = HashMap::new();
    for event in &events {
        latest
            .entry(event.pubkey)
            .and_modify(|current| {
                if event.created_at > current.created_at {
                    *current = event;
                }
            })
            .or_insert(event);
    }
    Ok(latest
        .into_iter()
        .filter_map(|(pubkey, event)| {
            Metadata::from_json(&event.content)
                .ok()
                .and_then(|metadata| metadata.nip05)
                .map(|nip05| (pubkey, nip05))
        })
        .collect())
}

/// Fetches quoted 9843/9840 requests the local cache lacks.
///
/// A failure leaves the quote unavailable, which makes coverage partial; it
/// never fails the command and never counts against the run.
struct RelayQuoteFetcher<'a> {
    client: &'a Client,
    relays: Vec<String>,
}

#[async_trait]
impl QuotedEventFetcher for RelayQuoteFetcher<'_> {
    async fn fetch(&self, wanted: &[WantedQuote]) -> Result<Vec<Event>> {
        let mut relays = self.relays.clone();
        for quote in wanted {
            if let Some(hint) = &quote.relay {
                if !relays.contains(hint) {
                    relays.push(hint.clone());
                }
            }
        }
        if relays.is_empty() {
            return Ok(Vec::new());
        }
        // The kind and author hints narrow the query; everything returned is
        // still verified in `validate_run_provenance`.
        let filter = Filter::default()
            .ids(wanted.iter().map(|quote| quote.event_id))
            .kinds(wanted.iter().map(|quote| quote.kind).collect::<Vec<Kind>>())
            .authors(wanted.iter().map(|quote| quote.requester));
        self.client.get_events(relays, vec![filter]).await
    }
}

fn short(hex: &str) -> String {
    hex.chars().take(8).collect()
}

/// Pubkeys are bech32 `npub`s in JSON, as on every other ngit surface.
fn npub(pubkey: PublicKey) -> String {
    pubkey.to_bech32().unwrap_or_else(|_| pubkey.to_hex())
}

/// Repository relays that answered nothing at all leave this view partial.
///
/// `FetchReport::state_per_relay` records every relay the repository-scoped
/// fetch reached, whether or not it held a state event; a relay whose fetch
/// errored never reaches the consolidated report.
fn relay_coverage(
    relays: &[RelayUrl],
    state_per_relay: &HashMap<RelayUrl, Option<Event>>,
) -> Coverage {
    if relays
        .iter()
        .all(|relay| state_per_relay.contains_key(relay))
    {
        Coverage::Complete
    } else {
        Coverage::Partial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(states: &[RunState]) -> Vec<RunReport> {
        states
            .iter()
            .map(|state| RunReport {
                run: WorkflowRun {
                    run_id: "run".to_owned(),
                    coordinator: nostr::prelude::Keys::generate().public_key(),
                    workflow_path: "ci.yml".to_owned(),
                    workflow_hash: String::new(),
                    trigger: kinds::Trigger::Push,
                    pr_root: None,
                    supplying_event: None,
                    git_ref: None,
                    commits: Vec::new(),
                    repositories: Vec::new(),
                    progress: None,
                    result: None,
                    jobs: Vec::new(),
                },
                state: *state,
                attempt_of: 1,
                resolution: TrustResolution::Settled {
                    classification: TrustClassification::NoKnownContext,
                    evidence: Vec::new(),
                    coverage: Coverage::Complete,
                },
                integrity: Integrity {
                    commit_present: false,
                    workflow_hash_matches: None,
                },
                jobs: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn an_unexpired_marker_makes_the_target_running_even_beside_a_conclusion() {
        assert_eq!(
            ci_state(&report(&[
                RunState::Concluded(Conclusion::Success),
                RunState::Running
            ])),
            CiState::Running
        );
    }

    #[test]
    fn only_expired_progress_is_stale_and_no_runs_is_none() {
        assert_eq!(ci_state(&report(&[RunState::Stale])), CiState::Stale);
        assert_eq!(ci_state(&report(&[])), CiState::None);
    }

    #[test]
    fn the_rollup_conclusion_is_the_worst_of_the_current_runs() {
        assert_eq!(
            rollup_conclusion(&report(&[
                RunState::Concluded(Conclusion::Success),
                RunState::Concluded(Conclusion::Cancelled),
                RunState::Concluded(Conclusion::Neutral),
            ])),
            Some(Conclusion::Cancelled)
        );
        assert_eq!(
            rollup_conclusion(&report(&[
                RunState::Concluded(Conclusion::Cancelled),
                RunState::Concluded(Conclusion::TimedOut),
            ])),
            Some(Conclusion::TimedOut)
        );
        assert_eq!(
            rollup_conclusion(&report(&[
                RunState::Concluded(Conclusion::Success),
                RunState::Concluded(Conclusion::Skipped),
            ])),
            Some(Conclusion::Success),
            "neutral and skipped never displace a real outcome"
        );
    }

    #[test]
    fn the_trust_floor_admits_stronger_evidence_only() {
        assert!(meets_floor(
            TrustClassification::MaintainerDirected,
            CiTrustFloor::OperationallyAssociated
        ));
        assert!(!meets_floor(
            TrustClassification::OperationallyAssociated,
            CiTrustFloor::MaintainerDirected
        ));
        assert!(!meets_floor(
            TrustClassification::SociallyCorroborated,
            CiTrustFloor::OperationallyAssociated
        ));
        assert!(!meets_floor(
            TrustClassification::NoKnownContext,
            CiTrustFloor::OperationallyAssociated
        ));
    }

    fn event(kind: Kind, tags: Vec<nostr::prelude::Tag>) -> Event {
        use nostr::prelude::event::FinalizeEvent;
        nostr::prelude::EventBuilder::new(kind, "")
            .tags(tags)
            .finalize(&nostr::prelude::Keys::generate())
            .expect("test event finalizes")
    }

    fn tag(values: &[&str]) -> nostr::prelude::Tag {
        nostr::prelude::Tag::parse(
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
        )
        .expect("test tag parses")
    }

    #[test]
    fn a_patch_upgraded_to_a_pr_anchors_on_the_upgrade_root() {
        // The thread root stays the patch; ngit's `pr_upgrade_root` rule
        // decides which event later updates — and CI `E` tags — reference.
        let patch_root = event(Kind::GitPatch, vec![tag(&["t", "root"])]);
        let upgrade_root = event(
            ngit::git_events::KIND_PULL_REQUEST,
            vec![tag(&["e", &patch_root.id.to_hex()])],
        );
        assert!(
            event_is_revision_root(&upgrade_root),
            "the upgrade root is a revision root, so it is not a thread root"
        );
        assert_eq!(
            pr_upgrade_root(&[patch_root.clone(), upgrade_root.clone()]).map(|event| event.id),
            Some(upgrade_root.id),
        );
        assert_eq!(
            root_reference(&upgrade_root),
            Some(patch_root.id),
            "an unmarked `e` is how an upgrade root names the patch it upgrades"
        );
    }

    #[test]
    fn a_revision_names_its_root_through_the_strongest_reference_it_carries() {
        let root = event(Kind::GitPatch, vec![tag(&["t", "root"])]);
        let update = event(
            KIND_PULL_REQUEST_UPDATE,
            vec![
                tag(&["E", &root.id.to_hex()]),
                tag(&["e", &"cc".repeat(32)]),
            ],
        );
        assert_eq!(root_reference(&update), Some(root.id), "`E` wins");

        let revision = event(
            Kind::GitPatch,
            vec![
                tag(&["t", "revision-root"]),
                tag(&["e", &"dd".repeat(32), "", "reply"]),
                tag(&["e", &root.id.to_hex(), "", "root"]),
            ],
        );
        assert_eq!(
            root_reference(&revision),
            Some(root.id),
            "a marked root `e` wins over an unmarked one"
        );
    }

    #[test]
    fn a_repository_relay_that_did_not_answer_makes_the_view_partial() {
        let answered = RelayUrl::parse("ws://answered.example").expect("valid url");
        let silent = RelayUrl::parse("ws://silent.example").expect("valid url");
        let mut state_per_relay = HashMap::new();
        state_per_relay.insert(answered.clone(), None);

        assert_eq!(
            relay_coverage(std::slice::from_ref(&answered), &state_per_relay),
            Coverage::Complete,
            "a relay that answered without a state event still answered"
        );
        assert_eq!(
            relay_coverage(&[answered, silent], &state_per_relay),
            Coverage::Partial
        );
        assert_eq!(relay_coverage(&[], &state_per_relay), Coverage::Complete);
    }

    #[test]
    fn a_short_hex_is_a_prefix_but_a_full_id_is_not() {
        assert!(is_hex_prefix("dead"));
        assert!(!is_hex_prefix(&"a".repeat(64)));
        assert!(!is_hex_prefix("feature-1"));
        assert!(!is_hex_prefix(""));
    }
}
