//! Grouping validated CI events into workflow runs.
//!
//! A run is identified by `(coordinator pubkey, workflow-run id)`: the
//! Workflow Result's non-`refs/` `r` value, the Workflow Progress `d` value
//! and the run id in a Job Result's quoted `39842:` address are the same
//! string. A run may have progress only, a result only, or both.
//!
//! Nothing here reads the clock. Expiry and run state take an explicit `now`
//! so the temporal rules are unit testable.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fmt,
};

use nostr::prelude::{Event, EventId, Kind, PublicKey, Timestamp};

use super::{
    kinds::{
        self, CiEvent, Conclusion, JobResult, ProvenanceQuote, RepoReference, ShapeReason, Trigger,
        WorkflowProgress, WorkflowResult,
    },
    total_order_position,
};

/// One workflow run attempt: its container events plus the Job Results that
/// name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    /// The workflow-run id, unique per attempt.
    pub run_id: String,
    /// The coordinator that signed the Result and/or Progress marker.
    pub coordinator: PublicKey,
    pub workflow_path: String,
    /// SHA-256 the coordinator claims for the workflow file at `commits[0]`.
    pub workflow_hash: String,
    pub trigger: Trigger,
    /// The kind-1618 pull request, for a `pull_request` run.
    pub pr_root: Option<EventId>,
    /// The 1618/1619 event that supplied the commit.
    pub supplying_event: Option<EventId>,
    /// The Git ref, for a push/tag run.
    pub git_ref: Option<String>,
    /// `c` tags: the peeled commit first.
    pub commits: Vec<String>,
    pub repositories: Vec<RepoReference>,
    pub progress: Option<WorkflowProgress>,
    pub result: Option<WorkflowResult>,
    /// Latest Job Result per job id, sorted by job id.
    pub jobs: Vec<JobResult>,
}

/// Whether a run is executing, finished, or abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// An unexpired queued/in-progress marker and no conclusion.
    Running,
    Concluded(Conclusion),
    /// Only an expired progress marker: the publisher stopped renewing it and
    /// never published a result.
    Stale,
}

impl WorkflowRun {
    /// The commit the workflow ran against.
    #[must_use]
    pub fn commit(&self) -> &str {
        self.commits.first().map_or("", String::as_str)
    }

    /// The event id of the run's container, preferring the immutable result.
    ///
    /// Grouping only builds a run from a Result or a Progress marker, so one
    /// of the two is always present.
    #[must_use]
    pub fn container_event_id(&self) -> EventId {
        self.result
            .as_ref()
            .map(|result| result.event_id)
            .or_else(|| self.progress.as_ref().map(|progress| progress.event_id))
            .expect("a run has a result or a progress marker")
    }

    #[must_use]
    pub fn container_created_at(&self) -> Timestamp {
        self.result
            .as_ref()
            .map(|result| result.created_at)
            .or_else(|| self.progress.as_ref().map(|progress| progress.created_at))
            .expect("a run has a result or a progress marker")
    }

    /// The run's frozen request-provenance quote, preferring the result's.
    ///
    /// The reference is unvalidated: a coordinator-authored `q` tag is not
    /// evidence until the quoted event has been fetched and checked.
    #[must_use]
    pub fn provenance(&self) -> Option<&ProvenanceQuote> {
        self.result
            .as_ref()
            .and_then(|result| result.provenance.as_ref())
            .or_else(|| {
                self.progress
                    .as_ref()
                    .and_then(|progress| progress.provenance.as_ref())
            })
    }

    /// When the run was handed off, for evaluating service-control coverage:
    /// `started_at` with `queued_at` as the fallback, preferring the result.
    ///
    /// `None` means coverage is indeterminate — it is never assumed.
    #[must_use]
    pub fn coverage_time(&self) -> Option<Timestamp> {
        let result = self.result.as_ref();
        let progress = self.progress.as_ref();
        result
            .and_then(|result| result.started_at)
            .or_else(|| progress.and_then(|progress| progress.started_at))
            .or_else(|| result.and_then(|result| result.queued_at))
            .or_else(|| progress.and_then(|progress| progress.queued_at))
    }

    /// The run's conclusion, from the result or a concluded progress marker.
    #[must_use]
    pub fn conclusion(&self) -> Option<Conclusion> {
        self.result
            .as_ref()
            .map(|result| result.conclusion)
            .or_else(|| {
                self.progress
                    .as_ref()
                    .and_then(|progress| progress.conclusion)
            })
    }

    /// Run state at `now`.
    ///
    /// A result concludes the run regardless of progress expiry. Otherwise a
    /// queued/in-progress marker only means "running" while it is unexpired:
    /// an expired marker with no result is stale, not running.
    #[must_use]
    pub fn state(&self, now: Timestamp) -> RunState {
        if let Some(conclusion) = self.conclusion() {
            return RunState::Concluded(conclusion);
        }
        match &self.progress {
            Some(progress) if progress.is_pending() && progress.is_live(now) => RunState::Running,
            _ => RunState::Stale,
        }
    }

    /// Position of this attempt in the NIP-01 total order, using `started_at`
    /// when the publisher provided it.
    #[must_use]
    pub fn attempt_position(&self) -> (Timestamp, Reverse<EventId>) {
        let started_at = self
            .result
            .as_ref()
            .and_then(|result| result.started_at)
            .or_else(|| self.progress.as_ref().and_then(|p| p.started_at));
        total_order_position(
            started_at.unwrap_or_else(|| self.container_created_at()),
            self.container_event_id(),
        )
    }

    /// Whether the run's Workflow Result quotes this Job Result.
    ///
    /// Coordinator trust reaches a separate provider only through the result
    /// that accepts the job.
    #[must_use]
    pub fn result_accepts_job(&self, job: &JobResult) -> bool {
        self.result.as_ref().is_some_and(|result| {
            result
                .jobs
                .iter()
                .any(|quote| quote.event_id == job.event_id)
        })
    }
}

/// Why an event contributed nothing to the grouped runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The event does not match the shape its kind requires.
    Shape(ShapeReason),
    /// A Job Result whose quoted run has neither a Result nor a Progress
    /// marker. The container is what associates a run with a repository, so
    /// an unquoted job is never promoted to a run of its own.
    OrphanedJobResult,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape(reason) => reason.fmt(f),
            Self::OrphanedJobResult => {
                write!(f, "no Workflow Result or Progress for the quoted run")
            }
        }
    }
}

/// An event that was not grouped, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEvent {
    pub event_id: EventId,
    pub kind: Kind,
    pub reason: SkipReason,
}

impl fmt::Display for SkippedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "skipped kind-{} event {}: {}",
            self.kind.as_u16(),
            self.event_id,
            self.reason
        )
    }
}

/// Grouped runs plus every event that was skipped and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSet {
    pub runs: Vec<WorkflowRun>,
    pub skipped: Vec<SkippedEvent>,
}

/// Group Workflow Results, Progress markers and Job Results into runs.
///
/// Manual Triggers and Service Controls are not runs and are ignored;
/// [`crate::ci::controls`] consumes those. Events that fail shape validation
/// are reported in [`RunSet::skipped`] rather than reinterpreted.
#[must_use]
pub fn group_workflow_runs<'a>(events: impl IntoIterator<Item = &'a Event>) -> RunSet {
    let mut skipped: Vec<SkippedEvent> = Vec::new();
    let mut results: HashMap<RunKey, WorkflowResult> = HashMap::new();
    let mut progress: HashMap<RunKey, WorkflowProgress> = HashMap::new();
    // Keyed by job id *and* signer: a Job Result is the signer's own
    // execution claim, so one signer's event must never replace another's.
    let mut jobs: HashMap<RunKey, HashMap<(String, PublicKey), JobResult>> = HashMap::new();

    for event in events {
        if !kinds::CONSUMED_CI_KINDS.contains(&event.kind) {
            continue;
        }
        let validated = match kinds::validate(event) {
            Ok(validated) => validated,
            Err(reason) => {
                skipped.push(SkippedEvent {
                    event_id: event.id,
                    kind: event.kind,
                    reason: SkipReason::Shape(reason),
                });
                continue;
            }
        };
        match validated {
            CiEvent::WorkflowResult(result) => {
                let key = RunKey {
                    coordinator: result.author,
                    run_id: result.run_id.clone(),
                };
                keep_later(results.entry(key), result, |result| {
                    total_order_position(result.created_at, result.event_id)
                });
            }
            CiEvent::WorkflowProgress(marker) => {
                let key = RunKey {
                    coordinator: marker.author,
                    run_id: marker.run_id.clone(),
                };
                keep_later(progress.entry(key), marker, |marker| {
                    total_order_position(marker.created_at, marker.event_id)
                });
            }
            CiEvent::JobResult(job) => {
                let key = RunKey {
                    coordinator: job.run.coordinator,
                    run_id: job.run.run_id.clone(),
                };
                let job_key = (job.job_id.clone(), job.author);
                keep_later(jobs.entry(key).or_default().entry(job_key), job, |job| {
                    total_order_position(job.created_at, job.event_id)
                });
            }
            CiEvent::ManualTrigger(_) | CiEvent::ServiceControl(_) => {}
        }
    }

    let mut runs: Vec<WorkflowRun> = Vec::new();
    let mut keys: Vec<RunKey> = results.keys().cloned().collect();
    keys.extend(
        progress
            .keys()
            .filter(|key| !results.contains_key(*key))
            .cloned(),
    );
    for key in keys {
        let result = results.remove(&key);
        let marker = progress.remove(&key);
        let candidates: Vec<JobResult> = jobs
            .remove(&key)
            .map(|by_key| by_key.into_values().collect())
            .unwrap_or_default();
        let run_jobs = select_jobs(result.as_ref(), candidates);
        runs.push(build_run(key, result, marker, run_jobs));
    }

    // Every remaining job names a run with no container.
    for job in jobs.into_values().flat_map(HashMap::into_values) {
        skipped.push(SkippedEvent {
            event_id: job.event_id,
            kind: kinds::KIND_CI_JOB_RESULT,
            reason: SkipReason::OrphanedJobResult,
        });
    }

    runs.sort_by_key(|run| (Reverse(run.attempt_position()), run.run_id.clone()));
    skipped.sort_by_key(|skipped| skipped.event_id);
    RunSet { runs, skipped }
}

/// Choose which Job Results represent the run.
///
/// Where the coordinator's Workflow Result quotes a Job Result for a job id,
/// only accepted results represent that job: an unaccepted result from
/// another signer cannot displace or shadow it. Where nothing was accepted
/// for a job id, every signer's latest claim is surfaced so the caller can
/// see the competing claims rather than one silently winning.
fn select_jobs(result: Option<&WorkflowResult>, candidates: Vec<JobResult>) -> Vec<JobResult> {
    let accepted: HashSet<EventId> = result
        .map(|result| result.jobs.iter().map(|quote| quote.event_id).collect())
        .unwrap_or_default();
    let accepted_job_ids: HashSet<String> = candidates
        .iter()
        .filter(|job| accepted.contains(&job.event_id))
        .map(|job| job.job_id.clone())
        .collect();

    let mut jobs: Vec<JobResult> = candidates
        .into_iter()
        .filter(|job| !accepted_job_ids.contains(&job.job_id) || accepted.contains(&job.event_id))
        .collect();
    jobs.sort_by(|a, b| {
        a.job_id
            .cmp(&b.job_id)
            .then_with(|| a.author.to_hex().cmp(&b.author.to_hex()))
    });
    jobs
}

/// The latest attempt per `(coordinator, workflow path)`.
///
/// Attempts are ordered by `started_at` when present, otherwise by the
/// container's `created_at`, with the NIP-01 tie-break: at equal timestamps
/// the lexicographically lower event id is later.
///
/// This says nothing about *which* code was built. Callers that present a
/// current state must first partition runs by the revision or commit they
/// are describing; otherwise a newer attempt for an older revision would be
/// selected as current.
#[must_use]
pub fn latest_attempts(runs: &[WorkflowRun]) -> Vec<&WorkflowRun> {
    let mut latest: HashMap<(PublicKey, &str), &WorkflowRun> = HashMap::new();
    for run in runs {
        let key = (run.coordinator, run.workflow_path.as_str());
        latest
            .entry(key)
            .and_modify(|current| {
                if run.attempt_position() > current.attempt_position() {
                    *current = run;
                }
            })
            .or_insert(run);
    }
    let mut selected: Vec<&WorkflowRun> = latest.into_values().collect();
    selected.sort_by(|a, b| {
        a.workflow_path
            .cmp(&b.workflow_path)
            .then_with(|| a.coordinator.to_hex().cmp(&b.coordinator.to_hex()))
    });
    selected
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunKey {
    coordinator: PublicKey,
    run_id: String,
}

fn keep_later<T>(
    entry: std::collections::hash_map::Entry<'_, impl std::hash::Hash + Eq, T>,
    candidate: T,
    position: impl Fn(&T) -> (Timestamp, Reverse<EventId>),
) {
    use std::collections::hash_map::Entry;
    match entry {
        Entry::Vacant(vacant) => {
            vacant.insert(candidate);
        }
        Entry::Occupied(mut occupied) => {
            if position(&candidate) > position(occupied.get()) {
                occupied.insert(candidate);
            }
        }
    }
}

fn build_run(
    key: RunKey,
    result: Option<WorkflowResult>,
    progress: Option<WorkflowProgress>,
    jobs: Vec<JobResult>,
) -> WorkflowRun {
    // The immutable result is preferred for the run's context; a progress
    // marker supplies it while the run has not concluded.
    let common = result
        .as_ref()
        .map(|result| &result.common)
        .or_else(|| progress.as_ref().map(|progress| &progress.common))
        .expect("a run is built from a result or a progress marker");

    WorkflowRun {
        run_id: key.run_id,
        coordinator: key.coordinator,
        workflow_path: common.workflow.path.clone(),
        workflow_hash: common.workflow.sha256.clone(),
        // Results and Progress markers are validated with the result-like
        // profile, which requires the normalized `o` trigger.
        trigger: common
            .trigger
            .expect("a result or progress marker carries an `o` trigger"),
        pr_root: common.pr_root(),
        supplying_event: common.supplying_event(),
        git_ref: common.git_ref().map(ToOwned::to_owned),
        commits: common.commits.clone(),
        repositories: common.repositories.clone(),
        progress,
        result,
        jobs,
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::Keys;

    use super::{
        super::kinds::{ProgressStatus, test_events::*},
        *,
    };

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_secs(secs)
    }

    #[test]
    fn groups_progress_result_and_jobs_by_run_id() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let events = vec![
            workflow_progress(
                &coordinator,
                &owner,
                "run-1",
                ProgressStatus::InProgress,
                100,
                700,
            ),
            workflow_result(&coordinator, &owner, "run-1", 200),
            job_result(&provider, &coordinator, &owner, "run-1", "build", 150),
            job_result(&provider, &coordinator, &owner, "run-1", "test", 160),
        ];
        let grouped = group_workflow_runs(&events);
        assert!(grouped.skipped.is_empty(), "{:?}", grouped.skipped);
        assert_eq!(grouped.runs.len(), 1);
        let run = &grouped.runs[0];
        assert_eq!(run.run_id, "run-1");
        assert_eq!(run.coordinator, coordinator.public_key());
        assert_eq!(run.workflow_path, WORKFLOW_PATH);
        assert_eq!(run.commit(), COMMIT);
        assert!(run.result.is_some());
        assert!(run.progress.is_some());
        assert_eq!(
            run.jobs
                .iter()
                .map(|job| job.job_id.as_str())
                .collect::<Vec<_>>(),
            vec!["build", "test"]
        );
    }

    #[test]
    fn separate_run_ids_are_separate_attempts() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![
            workflow_result(&coordinator, &owner, "run-1", 100),
            workflow_result(&coordinator, &owner, "run-2", 200),
        ];
        let grouped = group_workflow_runs(&events);
        assert_eq!(grouped.runs.len(), 2);
    }

    #[test]
    fn latest_progress_replacement_wins_by_created_at() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![
            workflow_progress(
                &coordinator,
                &owner,
                "run-1",
                ProgressStatus::Queued,
                100,
                700,
            ),
            workflow_progress(
                &coordinator,
                &owner,
                "run-1",
                ProgressStatus::InProgress,
                150,
                750,
            ),
        ];
        let grouped = group_workflow_runs(&events);
        assert_eq!(grouped.runs.len(), 1);
        assert_eq!(
            grouped.runs[0].progress.as_ref().unwrap().status,
            ProgressStatus::InProgress
        );
    }

    #[test]
    fn latest_attempt_prefers_lower_event_id_at_equal_timestamps() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let a = workflow_result(&coordinator, &owner, "run-a", 100);
        let b = workflow_result(&coordinator, &owner, "run-b", 100);
        let expected = if a.id < b.id { &a } else { &b };

        let grouped = group_workflow_runs(&[a.clone(), b.clone()]);
        let current = latest_attempts(&grouped.runs);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].container_event_id(), expected.id);
    }

    #[test]
    fn latest_attempt_prefers_the_newer_started_at() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![
            workflow_result(&coordinator, &owner, "run-1", 100),
            workflow_result(&coordinator, &owner, "run-2", 300),
        ];
        let grouped = group_workflow_runs(&events);
        let current = latest_attempts(&grouped.runs);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].run_id, "run-2");
    }

    #[test]
    fn attempts_from_different_coordinators_are_both_current() {
        let owner = Keys::generate();
        let first = Keys::generate();
        let second = Keys::generate();
        let events = vec![
            workflow_result(&first, &owner, "run-1", 100),
            workflow_result(&second, &owner, "run-2", 100),
        ];
        let grouped = group_workflow_runs(&events);
        assert_eq!(latest_attempts(&grouped.runs).len(), 2);
    }

    #[test]
    fn unexpired_pending_progress_is_running() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::Queued,
            100,
            700,
        )];
        let grouped = group_workflow_runs(&events);
        assert_eq!(grouped.runs[0].state(ts(699)), RunState::Running);
    }

    #[test]
    fn expired_progress_without_a_result_is_stale() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::InProgress,
            100,
            700,
        )];
        let grouped = group_workflow_runs(&events);
        assert_eq!(grouped.runs[0].state(ts(700)), RunState::Stale);
        assert_eq!(grouped.runs[0].state(ts(5_000)), RunState::Stale);
    }

    #[test]
    fn a_result_concludes_the_run_regardless_of_progress_expiry() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![
            workflow_progress(
                &coordinator,
                &owner,
                "run-1",
                ProgressStatus::InProgress,
                100,
                700,
            ),
            workflow_result(&coordinator, &owner, "run-1", 200),
        ];
        let grouped = group_workflow_runs(&events);
        assert_eq!(
            grouped.runs[0].state(ts(5_000)),
            RunState::Concluded(Conclusion::Success)
        );
    }

    #[test]
    fn concluded_progress_without_a_result_reports_its_conclusion() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let events = vec![workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::Concluded,
            100,
            700,
        )];
        let grouped = group_workflow_runs(&events);
        assert_eq!(
            grouped.runs[0].state(ts(120)),
            RunState::Concluded(Conclusion::Success)
        );
    }

    #[test]
    fn coverage_time_prefers_started_at_then_queued_at() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let with_started = workflow_result(&coordinator, &owner, "run-1", 100);
        let grouped = group_workflow_runs(std::slice::from_ref(&with_started));
        assert_eq!(grouped.runs[0].coverage_time(), Some(ts(100)));

        let queued_only = with_tags(&coordinator, &with_started, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("started_at"));
            tags.push(tag(&["queued_at", "60"]));
        });
        let grouped = group_workflow_runs(&[queued_only]);
        assert_eq!(grouped.runs[0].coverage_time(), Some(ts(60)));
    }

    #[test]
    fn coverage_time_is_indeterminate_without_timestamps() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let without = with_tags(&coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("started_at"));
        });
        let grouped = group_workflow_runs(&[without]);
        assert_eq!(grouped.runs[0].coverage_time(), None);
    }

    #[test]
    fn orphaned_job_results_are_skipped_with_a_reason() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let job = job_result(&provider, &coordinator, &owner, "run-1", "build", 150);
        let grouped = group_workflow_runs(std::slice::from_ref(&job));
        assert!(grouped.runs.is_empty());
        assert_eq!(
            grouped.skipped,
            vec![SkippedEvent {
                event_id: job.id,
                kind: kinds::KIND_CI_JOB_RESULT,
                reason: SkipReason::OrphanedJobResult,
            }]
        );
    }

    #[test]
    fn malformed_events_are_skipped_with_their_shape_reason() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let malformed = with_tags(&coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("conclusion"));
        });
        let grouped = group_workflow_runs(std::slice::from_ref(&malformed));
        assert!(grouped.runs.is_empty());
        assert_eq!(
            grouped.skipped,
            vec![SkippedEvent {
                event_id: malformed.id,
                kind: kinds::KIND_CI_WORKFLOW_RESULT,
                reason: SkipReason::Shape(ShapeReason::MissingTag("conclusion")),
            }]
        );
    }

    #[test]
    fn an_unaccepted_job_result_cannot_displace_the_accepted_one() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let impostor = Keys::generate();
        let owner = Keys::generate();
        let job = job_result(&provider, &coordinator, &owner, "run-1", "build", 150);
        // A stranger publishes a newer Job Result for the same job id.
        let forged = job_result(&impostor, &coordinator, &owner, "run-1", "build", 300);
        let base = workflow_result(&coordinator, &owner, "run-1", 200);
        let result = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&[
                "q",
                &job.id.to_hex(),
                "wss://relay.example",
                &provider.public_key().to_hex(),
                "build",
            ]));
        });

        let grouped = group_workflow_runs(&[forged, job.clone(), result]);
        let run = &grouped.runs[0];
        assert_eq!(run.jobs.len(), 1);
        assert_eq!(run.jobs[0].event_id, job.id);
        assert_eq!(run.jobs[0].author, provider.public_key());
    }

    #[test]
    fn competing_unaccepted_job_claims_are_both_surfaced() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let stranger = Keys::generate();
        let owner = Keys::generate();
        let events = vec![
            job_result(&provider, &coordinator, &owner, "run-1", "build", 150),
            job_result(&stranger, &coordinator, &owner, "run-1", "build", 300),
            workflow_result(&coordinator, &owner, "run-1", 200),
        ];

        let grouped = group_workflow_runs(&events);
        let authors: HashSet<PublicKey> =
            grouped.runs[0].jobs.iter().map(|job| job.author).collect();
        assert_eq!(
            authors,
            HashSet::from([provider.public_key(), stranger.public_key()]),
            "neither claim is hidden when the coordinator accepted neither"
        );
    }

    #[test]
    fn result_accepts_job_requires_a_quote_from_the_result() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let job = job_result(&provider, &coordinator, &owner, "run-1", "build", 150);
        let base = workflow_result(&coordinator, &owner, "run-1", 200);
        let quoting = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&[
                "q",
                &job.id.to_hex(),
                "wss://relay.example",
                &provider.public_key().to_hex(),
                "build",
            ]));
        });

        let unquoted = group_workflow_runs(&[job.clone(), base]);
        assert!(!unquoted.runs[0].result_accepts_job(&unquoted.runs[0].jobs[0]));

        let quoted = group_workflow_runs(&[job, quoting]);
        assert!(quoted.runs[0].result_accepts_job(&quoted.runs[0].jobs[0]));
    }
}
