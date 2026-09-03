//! The CI projection shared by `ngit ci status`, `ngit pr view` and
//! `ngit pr list`.
//!
//! One target, one document: every surface groups the same cached CI events
//! into runs, applies the same latest-revision rule and state machine, labels
//! every signer with the trust model in [`ngit::ci`], and renders the same
//! `ci` JSON object. Only the tier differs — `pr list` never leaves the local
//! cache, while the detail surfaces fetch missing quoted requests and verify
//! NIP-05 identities.
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

use anyhow::Result;
use async_trait::async_trait;
use bitcoin_hashes::sha256;
use ngit::{
    ci::{
        domain::{NetworkNip05Lookup, Nip05Cache},
        events::{RunState, WorkflowRun, group_workflow_runs, latest_attempts},
        kinds::{self, CONSUMED_CI_KINDS, Conclusion, JobResult, ServiceControl},
        provenance::{WantedQuote, wanted_quotes},
        resolve::{
            CiInputs, CiTrustContext, QuotedEventFetcher, RepositoryContext,
            identity_lookup_signers, resolve_cache_tier, resolve_full_tier,
        },
        trust::{CONTEXT_INCOMPLETE_LABEL, Coverage, TrustClassification, TrustResolution},
    },
    client::{
        Client, Connect, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_events_from_local_cache,
    },
    event_ordering::latest_event,
    git::Repo,
    git_events::KIND_PULL_REQUEST_UPDATE,
    repo_ref::RepoRef,
    utils::pr_upgrade_root,
};
use nostr::prelude::{
    Event, EventId, Filter, Kind, Metadata, PublicKey, RelayUrl, SingleLetterTag, Timestamp,
    ToBech32,
};
use serde_json::{Value, json};

use crate::{ci_commit::first_local_commit, cli::CiTrustFloor};

/// The trust floor a surface applies when the caller demanded none.
///
/// `pr list`'s `✓` and `ngit pr merge`'s non-blocking warning both need a
/// floor without one being named on the command line. They share this one so
/// a row that renders `✓` is never a merge that warns.
pub const DEFAULT_TRUST_FLOOR: CiTrustFloor = CiTrustFloor::OperationallyAssociated;

/// How much work a surface is willing to do to settle its evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The local cache alone: no relay fetch, no NIP-05 lookup. What
    /// `pr list` and `--offline` use.
    Cache,
    /// Additionally fetch missing quoted requests and verify NIP-05
    /// identities. What the detail surfaces use.
    Full,
}

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/// What a surface is describing, after resolution.
#[derive(Debug, Clone)]
pub enum Target {
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

/// The runs a target presents as current, those it presents as outdated, and
/// whether any run it has CI for describes its current revision.
pub struct Selection {
    /// Runs for the target's latest revision.
    pub current: Vec<WorkflowRun>,
    /// Runs for an earlier revision. Never empty for a commit target: a
    /// commit has no revisions.
    pub outdated: Vec<WorkflowRun>,
    pub revision_matched: bool,
}

impl Target {
    /// The kind-1618 event this target's CI anchors on, for a PR.
    #[must_use]
    pub fn anchor(&self) -> Option<EventId> {
        match self {
            Self::PullRequest { anchor, .. } => Some(*anchor),
            Self::Commit { .. } => None,
        }
    }

    /// The cache query for this target's CI events.
    #[must_use]
    pub fn ci_filter(&self) -> Filter {
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

    /// Partition `runs` into this target's current and outdated runs.
    ///
    /// A PR presents only the runs for its latest revision: an attempt for an
    /// earlier revision is never current, even when it is newer. `runs` may
    /// describe other targets too — `pr list` groups one batched query for
    /// every listed PR — so the split is by this target's own anchor.
    #[must_use]
    pub fn select(&self, runs: &[WorkflowRun]) -> Selection {
        match self {
            Self::PullRequest {
                anchor, revision, ..
            } => {
                let mine: Vec<&WorkflowRun> = runs
                    .iter()
                    .filter(|run| run.pr_root == Some(*anchor))
                    .collect();
                let is_current = |run: &WorkflowRun| {
                    run.supplying_event
                        .map_or(*revision == *anchor, |supplying| supplying == *revision)
                };
                let current: Vec<WorkflowRun> = mine
                    .iter()
                    .filter(|run| is_current(run))
                    .map(|run| (*run).clone())
                    .collect();
                let outdated: Vec<WorkflowRun> = mine
                    .iter()
                    .filter(|run| !is_current(run))
                    .map(|run| (*run).clone())
                    .collect();
                Selection {
                    // Nothing at all is "no CI", not "CI for something else".
                    revision_matched: !current.is_empty() || mine.is_empty(),
                    current,
                    outdated,
                }
            }
            Self::Commit { ids, .. } => {
                let current: Vec<WorkflowRun> = runs
                    .iter()
                    .filter(|run| {
                        run.commits
                            .iter()
                            .any(|commit| ids.iter().any(|id| id.eq_ignore_ascii_case(commit)))
                    })
                    .cloned()
                    .collect();
                Selection {
                    revision_matched: !current.is_empty() || runs.is_empty(),
                    current,
                    outdated: Vec::new(),
                }
            }
        }
    }

    #[must_use]
    pub fn describe(&self) -> String {
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
    #[must_use]
    pub fn to_json(&self, relay: Option<&RelayUrl>) -> Value {
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

/// Build a PR target: the event CI anchors on, and the revision that is
/// current for it.
///
/// A thread that started as patches and was upgraded to a PR keeps the patch
/// root as its ngit identity, while every later PR update — and every CI
/// event — references the kind-1618 upgrade root. `utils::pr_upgrade_root`
/// is the one rule that decides which event that is.
///
/// # Errors
///
/// Returns an error when the thread cannot be read from the local cache.
pub async fn pull_request_target(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    root: EventId,
) -> Result<Target> {
    let thread =
        get_all_proposal_patch_pr_pr_update_events_from_cache(git_repo_path, repo_ref, &root)
            .await?;
    let anchor = pr_upgrade_root(&thread).map_or(root, |event| event.id);
    let revision = latest_event(
        thread
            .iter()
            .filter(|event| event.kind == KIND_PULL_REQUEST_UPDATE),
    )
    .map_or(anchor, |event| event.id);
    Ok(Target::PullRequest {
        root,
        anchor,
        revision,
    })
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Whether the target's current runs are executing, finished, or abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    /// At least one current run has a live progress marker.
    Running,
    /// *Every* current run concluded. Anything less has not finished.
    Concluded,
    /// Nothing is running and at least one current run never concluded.
    Stale,
    /// No current run at all.
    None,
}

impl CiState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
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
pub struct Integrity {
    pub commit_present: bool,
    pub workflow_hash_matches: Option<bool>,
}

pub struct JobReport {
    pub job_id: String,
    pub conclusion: Conclusion,
    pub provider: PublicKey,
    pub resolution: TrustResolution,
}

pub struct RunReport {
    pub run: WorkflowRun,
    pub state: RunState,
    /// 1-based position of this attempt among the attempts seen for the same
    /// coordinator and workflow on this target's revision.
    pub attempt_of: usize,
    pub resolution: TrustResolution,
    pub integrity: Integrity,
    pub jobs: Vec<JobReport>,
}

pub struct CiReport {
    pub state: CiState,
    pub conclusion: Option<Conclusion>,
    pub revision_matched: bool,
    pub coverage: Coverage,
    /// The conservative rollup across the current runs.
    pub rollup: TrustResolution,
    pub runs: Vec<RunReport>,
    /// Runs for an earlier revision, when the surface asked for them. They
    /// are never presented as current: `state`, `conclusion` and `rollup`
    /// describe `runs` alone.
    pub outdated: Option<Vec<RunReport>>,
    /// Events that were not grouped, with the reason.
    pub skipped: Vec<String>,
}

impl CiReport {
    #[must_use]
    pub fn is_incomplete(&self) -> bool {
        self.coverage != Coverage::Complete
    }

    /// Whether there is anything to show: a current run, or an outdated one.
    #[must_use]
    pub fn has_results(&self) -> bool {
        !self.runs.is_empty()
            || self
                .outdated
                .as_ref()
                .is_some_and(|outdated| !outdated.is_empty())
    }

    /// How the current result falls short of `floor`, or `None` when it does
    /// not.
    ///
    /// The rollup this reads is computed from the current runs alone, over
    /// classifications that saw every run known for the target. Every surface
    /// that acts on a trust floor — the `--require-ci-trust` gate and
    /// `pr merge`'s warning — goes through here, so they cannot disagree
    /// about what "below the floor" means.
    ///
    /// What counts as a green conclusion is [`is_green`], the same predicate
    /// the `pr list` glyph renders, so the gate and the glyphs cannot
    /// disagree about that either.
    #[must_use]
    pub fn shortfall(&self, floor: CiTrustFloor) -> Option<String> {
        if self.state != CiState::Concluded {
            return Some(format!(
                "CI has not concluded for this target (state: {})",
                self.state.as_str(),
            ));
        }
        if !self.conclusion.is_some_and(is_green) {
            return Some(format!(
                "CI concluded {}",
                self.conclusion.map_or("unknown", Conclusion::as_str),
            ));
        }
        // Both tiers always settle, so a classification is always present.
        // An unsettled one would be a caller rendering `CiTrustContext::
        // loading()`, and a floor is a claim about evidence: with none
        // assembled there is nothing to meet it.
        let Some(classification) = self.rollup.classification() else {
            return Some("the CI trust context did not settle".to_owned());
        };
        if meets_floor(classification, floor) {
            return None;
        }
        Some(format!(
            "the weakest current run is {}",
            classification.label(),
        ))
    }

    /// Why `--require-ci-trust` refuses, or `None` when the gate passes.
    #[must_use]
    pub fn gate_failure(&self, floor: CiTrustFloor) -> Option<String> {
        self.shortfall(floor)
            .map(|reason| format!("--require-ci-trust={}: {reason}", floor.as_str()))
    }

    /// The non-blocking caveat `ngit pr merge` prints when no floor was
    /// demanded, or `None` when there is nothing to say.
    ///
    /// The same shortfall the gate refuses on, measured against
    /// [`DEFAULT_TRUST_FLOOR`], with one exception: a target CI was never
    /// asked about is not a failing, unfinished or weakly-signed result, and
    /// warning there would fire on every merge in every repository without
    /// CI.
    ///
    /// "Never asked about" is `state: "none"` *and* `revision_matched`. CI
    /// that ran only for a superseded revision is also `none` — an earlier
    /// revision's result is never presented as current — but it is precisely
    /// the case a merging maintainer must not be left to infer from silence.
    #[must_use]
    pub fn merge_warning(&self) -> Option<String> {
        if self.state == CiState::None {
            return (!self.revision_matched).then(|| {
                "CI results exist for this PR but not for the revision being merged".to_owned()
            });
        }
        self.shortfall(DEFAULT_TRUST_FLOOR)
    }

    /// The `ci` object every surface embeds.
    ///
    /// `outdated` is present only where the surface groups by revision
    /// (`ngit pr view`); each of its entries carries the `revision` that
    /// supplied it, so an earlier revision's result can never be mistaken for
    /// the current one.
    #[must_use]
    pub fn to_ci_value(&self, relay: Option<&RelayUrl>) -> Value {
        let mut ci = json!({
            "state": self.state.as_str(),
            "conclusion": self.conclusion.map(Conclusion::as_str),
            "revision_matched": self.revision_matched,
            "coverage": self.coverage.as_str(),
            "runs": self.runs.iter().map(RunReport::to_json).collect::<Vec<Value>>(),
        });
        if let Some(outdated) = &self.outdated {
            ci["outdated"] = outdated
                .iter()
                .map(|report| {
                    let mut value = report.to_json();
                    value["revision"] = report.run.supplying_event.map_or(Value::Null, |id| {
                        json!(crate::output::event_id_to_nevent(id, relay))
                    });
                    value
                })
                .collect::<Vec<Value>>()
                .into();
        }
        if !self.skipped.is_empty() {
            ci["skipped"] = json!(self.skipped);
        }
        ci
    }

    /// The whole `ngit ci status` document.
    #[must_use]
    pub fn to_json(
        &self,
        target: &Target,
        relay: Option<&RelayUrl>,
        gate_failure: Option<&str>,
    ) -> Value {
        let mut document = json!({
            "command_status": if gate_failure.is_some() { "error" } else { "ok" },
            "entity": "ci",
            "target": target.to_json(relay),
            "ci": self.to_ci_value(relay),
        });
        if let Some(reason) = gate_failure {
            document["error"] = json!(reason);
        }
        document
    }

    /// The `ngit ci status` human rendering.
    pub fn print(&self, target: &Target) {
        println!("CI for {}", target.describe());
        self.print_result_lines();
        // Shape rejections are diagnostics for a publisher, not something a
        // reader of this repository can act on, so they follow ngit's
        // verbosity idiom. They stay in the JSON document unconditionally.
        if ngit::output_mode::is_verbose() {
            for skipped in &self.skipped {
                println!("  {skipped}");
            }
        }
    }

    /// The `ngit pr view` "Checks" section.
    pub fn print_checks(&self) {
        println!();
        println!("Checks:");
        self.print_result_lines();
    }

    /// The check lines every surface shares: the current runs (or the
    /// no-results message), the rolled-up state, superseded revisions, and
    /// the coverage caveat. Framing — the heading, and `ci status`'s verbose
    /// skip diagnostics — stays with each surface.
    ///
    /// The caveat is guarded by [`Self::has_results`]: on a surface built
    /// without `include_outdated` (`ci status`, `pr merge`) `outdated` is
    /// `None`, so this is the "any current run" guard those surfaces always
    /// had.
    fn print_result_lines(&self) {
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
        self.print_outdated();
        if self.is_incomplete() && self.has_results() {
            println!("  {CONTEXT_INCOMPLETE_LABEL}");
        }
    }

    /// Earlier revisions, under their own heading. A result for a superseded
    /// revision says nothing about the code under review now, so it is never
    /// mixed into the lines above.
    fn print_outdated(&self) {
        let Some(outdated) = self.outdated.as_ref().filter(|runs| !runs.is_empty()) else {
            return;
        };
        println!("  outdated (earlier revisions):");
        for run in outdated {
            run.print();
        }
    }
}

impl RunReport {
    #[must_use]
    pub fn conclusion(&self) -> Option<Conclusion> {
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

    #[must_use]
    pub fn to_json(&self) -> Value {
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

    pub fn print(&self) {
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

/// What a surface wants projected.
pub struct ProjectionRequest<'a> {
    pub target: &'a Target,
    pub tier: Tier,
    /// Whether earlier revisions are collected under [`CiReport::outdated`].
    pub include_outdated: bool,
    /// Coverage the caller's own relay queries settled with.
    pub input_coverage: Coverage,
}

/// Project one target's CI events into the report every surface renders.
///
/// # Errors
///
/// Returns an error when the local cache cannot be read.
pub async fn build_report(
    git_repo: &Repo,
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    client: &Client,
    request: &ProjectionRequest<'_>,
) -> Result<CiReport> {
    let now = Timestamp::now();
    let repository = RepositoryContext::from_repo_ref(repo_ref);

    let events =
        get_events_from_local_cache(git_repo_path, vec![request.target.ci_filter()]).await?;
    let grouped = group_workflow_runs(&events);
    let skipped: Vec<String> = grouped
        .skipped
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let selection = request.target.select(&grouped.runs);
    let current = latest_attempts_with_positions(&selection.current);
    // Evidence assembly sees every run known for the target, whether or not
    // this surface reports the superseded ones: an earlier revision's run is
    // legitimate identity-level evidence about its signer, and a signer must
    // not be classified differently by `ci status` and `pr view`. State,
    // conclusion, rollup and display use `current` alone.
    let outdated = outdated_attempts(&selection.outdated);
    let described: Vec<WorkflowRun> = current
        .iter()
        .chain(outdated.iter())
        .map(|attempt| attempt.run.clone())
        .collect();

    let controls = load_service_controls(git_repo_path, &repository).await?;
    let wanted = wanted_quotes(&described);
    let quoted_events = load_quoted_events(git_repo_path, &wanted).await?;

    // Only the signers the full tier will actually resolve identities for: a
    // profile fetched for anyone else would feed a lookup that never happens.
    let signers: HashSet<PublicKey> =
        identity_lookup_signers(&described, &controls, &repository.confirmed_maintainers)
            .into_iter()
            .collect();
    if request.tier == Tier::Full {
        // The signer-declared identity route needs the signer's kind-0. CI
        // signers are not repository contributors, so the repository fetch
        // never asks for their profiles.
        fetch_signer_profiles(git_repo_path, client, repo_ref, &signers).await;
    }
    let profile_nip05 = load_profile_nip05(git_repo_path, &signers).await?;

    let mut inputs = CiInputs::new(
        &repository,
        &described,
        &controls,
        &quoted_events,
        &profile_nip05,
        now,
    );
    inputs.input_coverage = request.input_coverage;
    let context = match request.tier {
        Tier::Cache => resolve_cache_tier(&inputs),
        Tier::Full => {
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
        }
    };

    let report = |attempt: &Attempt| RunReport {
        state: attempt.run.state(now),
        attempt_of: attempt.attempt_of,
        resolution: context.run_resolution(&attempt.run),
        integrity: check_integrity(git_repo, &attempt.run),
        jobs: attempt
            .run
            .jobs
            .iter()
            .map(|job| job_report(&context, &attempt.run, job))
            .collect(),
        run: attempt.run.clone(),
    };
    let runs: Vec<RunReport> = current.iter().map(&report).collect();
    let outdated: Option<Vec<RunReport>> = request
        .include_outdated
        .then(|| outdated.iter().map(&report).collect());

    let states: Vec<RunState> = runs.iter().map(|run| run.state).collect();
    let state = ci_state(&states);
    let current_runs: Vec<WorkflowRun> = current
        .into_iter()
        .map(|attempt| attempt.run)
        .collect::<Vec<WorkflowRun>>();
    Ok(CiReport {
        state,
        conclusion: if state == CiState::Concluded {
            rollup_conclusion(&states)
        } else {
            None
        },
        revision_matched: selection.revision_matched,
        coverage: context.coverage().unwrap_or(Coverage::Partial),
        rollup: context.summarize(&current_runs),
        runs,
        outdated,
        skipped,
    })
}

// ---------------------------------------------------------------------------
// List projection
// ---------------------------------------------------------------------------

/// One `pr list` row's CI cell.
///
/// Computed from the cache tier alone: the list path performs no relay fetch
/// of its own and no NIP-05 lookup, so a signer's domain evidence is never
/// consulted here. That leaves coverage partial for every row that has a
/// signer at all, which is why it is reported in JSON rather than caveated
/// per row in the table.
pub struct ListCiRow {
    pub state: CiState,
    pub conclusion: Option<Conclusion>,
    /// `None` when there is no current run to classify — an absence of runs,
    /// not an absence of evidence about a signer.
    pub classification: Option<TrustClassification>,
    /// Whether the weakest current run is operationally associated or better.
    pub trust_floor_met: bool,
    /// `None` for the same reason: with no current run, nothing was left
    /// unchecked.
    pub coverage: Option<Coverage>,
    pub revision_matched: bool,
}

impl ListCiRow {
    /// The cell as rendered in the table.
    ///
    /// A failure is a prompt to look rather than a verdict, so it carries no
    /// trust qualifier; only a passing result distinguishes "trust floor met"
    /// from "the weakest signer behind it has no known context".
    ///
    /// Which conclusions pass is [`is_green`], the predicate
    /// [`CiReport::shortfall`] gates on, so `✓` is never a merge
    /// `--require-ci-trust=operationally-associated` would refuse.
    #[must_use]
    pub fn glyph(&self) -> &'static str {
        match self.state {
            CiState::Running => "…",
            CiState::Stale => "~",
            CiState::None => "-",
            CiState::Concluded => {
                if self.conclusion.is_some_and(is_green) {
                    if self.trust_floor_met { "✓" } else { "✓?" }
                } else {
                    "✗"
                }
            }
        }
    }

    /// Structured fields, never the glyph alone: a consumer should read the
    /// state and the trust floor rather than parse a symbol.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "conclusion": self.conclusion.map(Conclusion::as_str),
            "classification": self.classification.map(TrustClassification::as_str),
            "trust_floor_met": self.trust_floor_met,
            "coverage": self.coverage.map(Coverage::as_str),
            "revision_matched": self.revision_matched,
        })
    }
}

/// A row with no CI events at all: nothing ran, nothing to classify, nothing
/// left unchecked. The same encoding a PR whose thread could not be read
/// gets, so "no CI" has one JSON shape.
impl Default for ListCiRow {
    fn default() -> Self {
        Self {
            state: CiState::None,
            conclusion: None,
            classification: None,
            trust_floor_met: false,
            coverage: None,
            revision_matched: true,
        }
    }
}

/// The CI cell for every listed PR, from the local cache alone.
///
/// One `#E` query covers every target, and one trust context labels every
/// run, so the whole column costs three cache reads — runs, service controls,
/// quoted requests — rather than three per row.
///
/// # Errors
///
/// Returns an error when the local cache cannot be read.
pub async fn list_ci_rows(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    targets: &[(EventId, Target)],
    input_coverage: Coverage,
) -> Result<HashMap<EventId, ListCiRow>> {
    let now = Timestamp::now();
    let anchors: Vec<EventId> = targets
        .iter()
        .filter_map(|(_, target)| target.anchor())
        .collect();
    if anchors.is_empty() {
        return Ok(HashMap::new());
    }

    let events = get_events_from_local_cache(
        git_repo_path,
        vec![
            Filter::default()
                .kinds(CONSUMED_CI_KINDS.to_vec())
                .custom_tags(SingleLetterTag::UPPERCASE_E, anchors),
        ],
    )
    .await?;
    let grouped = group_workflow_runs(&events);

    // Attempts are collapsed once per target and reused: for the evidence
    // inputs below, and for the row itself.
    let per_target: Vec<TargetRuns> = targets
        .iter()
        .map(|(id, target)| TargetRuns::new(*id, &target.select(&grouped.runs)))
        .collect();
    // As in `build_report`: evidence assembly sees every run known for a
    // target, the row reports only the current ones.
    let described: Vec<WorkflowRun> = per_target
        .iter()
        .flat_map(|target| target.described.iter().cloned())
        .collect();

    let repository = RepositoryContext::from_repo_ref(repo_ref);
    let controls = load_service_controls(git_repo_path, &repository).await?;
    let quoted_events = load_quoted_events(git_repo_path, &wanted_quotes(&described)).await?;
    // The cache tier never consults a signer's declared identity: the domain
    // ladder it would feed is skipped by definition.
    let profile_nip05 = HashMap::new();
    let mut inputs = CiInputs::new(
        &repository,
        &described,
        &controls,
        &quoted_events,
        &profile_nip05,
        now,
    );
    inputs.input_coverage = input_coverage;
    let context = resolve_cache_tier(&inputs);

    Ok(per_target
        .into_iter()
        .map(|target| {
            if target.current.is_empty() {
                // No current run: "no CI", encoded exactly as a PR whose
                // thread could not be read is.
                return (
                    target.id,
                    ListCiRow {
                        revision_matched: target.revision_matched,
                        ..ListCiRow::default()
                    },
                );
            }
            let states: Vec<RunState> = target.current.iter().map(|run| run.state(now)).collect();
            let state = ci_state(&states);
            let rollup = context.summarize(&target.current);
            let classification = rollup.classification();
            (
                target.id,
                ListCiRow {
                    state,
                    conclusion: if state == CiState::Concluded {
                        rollup_conclusion(&states)
                    } else {
                        None
                    },
                    classification,
                    trust_floor_met: classification.is_some_and(|classification| {
                        meets_floor(classification, DEFAULT_TRUST_FLOOR)
                    }),
                    coverage: Some(
                        rollup
                            .coverage()
                            .unwrap_or(Coverage::Partial)
                            .combine(input_coverage),
                    ),
                    revision_matched: target.revision_matched,
                },
            )
        })
        .collect())
}

/// One listed PR's runs, collapsed to the attempts that represent them.
struct TargetRuns {
    id: EventId,
    /// The latest attempts of the current revision — what the row reports.
    current: Vec<WorkflowRun>,
    /// Those plus the latest attempts of every superseded revision — what
    /// evidence assembly sees.
    described: Vec<WorkflowRun>,
    revision_matched: bool,
}

impl TargetRuns {
    fn new(id: EventId, selection: &Selection) -> Self {
        let current: Vec<WorkflowRun> = latest_attempts_with_positions(&selection.current)
            .into_iter()
            .map(|attempt| attempt.run)
            .collect();
        let described: Vec<WorkflowRun> = current
            .iter()
            .cloned()
            .chain(
                outdated_attempts(&selection.outdated)
                    .into_iter()
                    .map(|attempt| attempt.run),
            )
            .collect();
        Self {
            id,
            current,
            described,
            revision_matched: selection.revision_matched,
        }
    }
}

fn job_report(context: &CiTrustContext, run: &WorkflowRun, job: &JobResult) -> JobReport {
    JobReport {
        job_id: job.job_id.clone(),
        conclusion: job.conclusion,
        provider: job.author,
        resolution: context.job_resolution(run, job),
    }
}

/// A current attempt and its position among the attempts for the same
/// `(coordinator, workflow)`.
struct Attempt {
    run: WorkflowRun,
    attempt_of: usize,
}

/// The latest attempt per `(coordinator, workflow)`, each with the number of
/// attempts it is the latest of.
fn latest_attempts_with_positions(runs: &[WorkflowRun]) -> Vec<Attempt> {
    let mut counts: HashMap<(PublicKey, &str), usize> = HashMap::new();
    for run in runs {
        *counts
            .entry((run.coordinator, run.workflow_path.as_str()))
            .or_insert(0) += 1;
    }
    latest_attempts(runs)
        .into_iter()
        .map(|run| Attempt {
            attempt_of: counts
                .get(&(run.coordinator, run.workflow_path.as_str()))
                .copied()
                .unwrap_or(1),
            run: run.clone(),
        })
        .collect()
}

/// The latest attempts of every superseded revision, newest revision first.
///
/// Attempts are collapsed *within* a revision, never across them:
/// `latest_attempts` keys on `(coordinator, workflow)` alone, so collapsing
/// the whole set would hide one revision's result behind another's.
fn outdated_attempts(runs: &[WorkflowRun]) -> Vec<Attempt> {
    let mut by_revision: HashMap<Option<EventId>, Vec<WorkflowRun>> = HashMap::new();
    for run in runs {
        by_revision
            .entry(run.supplying_event)
            .or_default()
            .push(run.clone());
    }
    let mut groups: Vec<(Option<EventId>, Vec<WorkflowRun>)> = by_revision.into_iter().collect();
    // Newest superseded revision first. A `HashMap` has no order of its own,
    // so equal timestamps are broken on the supplying event id: the listing
    // must not depend on iteration order.
    groups.sort_by_key(|(supplying, group)| {
        std::cmp::Reverse((
            group
                .iter()
                .map(WorkflowRun::container_created_at)
                .max()
                .unwrap_or_default(),
            supplying.map(|id| id.to_hex()),
        ))
    });
    groups
        .iter()
        .flat_map(|(_, group)| latest_attempts_with_positions(group))
        .collect()
}

/// The state machine over a target's current runs.
///
/// `Concluded` requires *every* current run to have concluded. One workflow
/// that succeeded beside another that was abandoned mid-flight is not a
/// finished target: reporting it as concluded would roll the survivors up
/// into a `success` and let `--require-ci-trust` pass a target whose CI never
/// completed. An unfinished run with no live marker anywhere leaves the
/// target `Stale`, which is the same "has not concluded" shortfall a
/// wholly-abandoned target reports.
fn ci_state(states: &[RunState]) -> CiState {
    if states.is_empty() {
        return CiState::None;
    }
    if states.contains(&RunState::Running) {
        return CiState::Running;
    }
    if states
        .iter()
        .all(|state| matches!(state, RunState::Concluded(_)))
    {
        return CiState::Concluded;
    }
    CiState::Stale
}

/// Worst-of rollup: `failure`/`timed_out`/`startup_failure` beat `cancelled`,
/// which beats `success`; `neutral` and `skipped` do not fail the rollup.
fn rollup_conclusion(states: &[RunState]) -> Option<Conclusion> {
    states
        .iter()
        .filter_map(|state| match state {
            RunState::Concluded(conclusion) => Some(*conclusion),
            RunState::Running | RunState::Stale => None,
        })
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

/// The one green predicate, read by every surface.
///
/// The `pr list` glyph calls it directly and [`CiReport::shortfall`] — the
/// `--require-ci-trust` gate on `ngit ci status` and `ngit pr merge`, and
/// `pr merge`'s unflagged warning — calls it for the rolled-up conclusion, so
/// a row that renders `✓` can never be a merge the gate refuses.
///
/// `neutral` and `skipped` are green. They are *concluded* runs reporting
/// that there was nothing to do, which is why the worst-of rollup already
/// ranks them below `success` rather than above it; a workflow that decided
/// it had no work must not be the reason a merge is blocked. `cancelled` is
/// not green: nothing ran to completion, so it is grouped with the outcomes
/// that ask for a look.
fn is_green(conclusion: Conclusion) -> bool {
    conclusion_severity(conclusion) <= conclusion_severity(Conclusion::Success)
}

pub fn meets_floor(classification: TrustClassification, floor: CiTrustFloor) -> bool {
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
    let Some(commit) = first_local_commit(git_repo, &run.commits) else {
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

pub fn short(hex: &str) -> String {
    hex.chars().take(8).collect()
}

/// Pubkeys are bech32 `npub`s in JSON, as on every other ngit surface.
fn npub(pubkey: PublicKey) -> String {
    pubkey.to_bech32().unwrap_or_else(|_| pubkey.to_hex())
}

/// Repository relays that answered nothing at all leave a view partial.
///
/// `FetchReport::state_per_relay` records every relay the repository-scoped
/// fetch reached, whether or not it held a state event; a relay whose fetch
/// errored never reaches the consolidated report.
#[must_use]
pub fn relay_coverage(
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
    use nostr::prelude::Keys;

    use super::*;

    #[test]
    fn an_unexpired_marker_makes_the_target_running_even_beside_a_conclusion() {
        assert_eq!(
            ci_state(&[RunState::Concluded(Conclusion::Success), RunState::Running]),
            CiState::Running
        );
    }

    #[test]
    fn only_expired_progress_is_stale_and_no_runs_is_none() {
        assert_eq!(ci_state(&[RunState::Stale]), CiState::Stale);
        assert_eq!(ci_state(&[]), CiState::None);
    }

    #[test]
    fn a_current_run_that_never_concluded_keeps_the_target_from_concluding() {
        let mixed = [RunState::Concluded(Conclusion::Success), RunState::Stale];
        assert_eq!(
            ci_state(&mixed),
            CiState::Stale,
            "a workflow that was abandoned mid-flight leaves the target \
             unfinished, whatever the workflows beside it reported"
        );
        // What that costs the gate: the surviving success is never rolled up
        // into a conclusion, so the strongest possible trust cannot carry it
        // over any floor.
        let report = rolled_up(
            ci_state(&mixed),
            None,
            TrustClassification::MaintainerDirected,
        );
        assert!(
            report
                .gate_failure(CiTrustFloor::OperationallyAssociated)
                .is_some(),
            "`--require-ci-trust` blocks until CI is green, and a target one \
             of whose workflows never completed is not green"
        );
        assert!(report.merge_warning().is_some());
    }

    #[test]
    fn the_rollup_conclusion_is_the_worst_of_the_current_runs() {
        assert_eq!(
            rollup_conclusion(&[
                RunState::Concluded(Conclusion::Success),
                RunState::Concluded(Conclusion::Cancelled),
                RunState::Concluded(Conclusion::Neutral),
            ]),
            Some(Conclusion::Cancelled)
        );
        assert_eq!(
            rollup_conclusion(&[
                RunState::Concluded(Conclusion::Cancelled),
                RunState::Concluded(Conclusion::TimedOut),
            ]),
            Some(Conclusion::TimedOut)
        );
        assert_eq!(
            rollup_conclusion(&[
                RunState::Concluded(Conclusion::Success),
                RunState::Concluded(Conclusion::Skipped),
            ]),
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
            TrustClassification::SeenInYourNetwork,
            CiTrustFloor::OperationallyAssociated
        ));
        assert!(!meets_floor(
            TrustClassification::NoKnownContext,
            CiTrustFloor::OperationallyAssociated
        ));
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

    fn row(state: CiState, conclusion: Option<Conclusion>, trust_floor_met: bool) -> ListCiRow {
        ListCiRow {
            state,
            conclusion,
            trust_floor_met,
            ..ListCiRow::default()
        }
    }

    #[test]
    fn the_list_glyph_qualifies_a_pass_but_never_a_failure() {
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Success), true).glyph(),
            "✓"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Success), false).glyph(),
            "✓?",
            "a pass signed only by an unknown signer is qualified"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Failure), false).glyph(),
            "✗"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Failure), true).glyph(),
            "✗",
            "a failure is a prompt to look at any trust level"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Cancelled), true).glyph(),
            "✗",
            "nothing ran to completion, so cancelled is not a pass"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Skipped), true).glyph(),
            "✓",
            "skipped and neutral do not fail the rollup"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Neutral), true).glyph(),
            "✓"
        );
        assert_eq!(
            row(CiState::Concluded, Some(Conclusion::Neutral), false).glyph(),
            "✓?",
            "a green non-success is qualified by trust like any other pass"
        );
        assert_eq!(row(CiState::Running, None, false).glyph(), "…");
        assert_eq!(row(CiState::Stale, None, false).glyph(), "~");
        assert_eq!(row(CiState::None, None, false).glyph(), "-");
    }

    /// A concluded pull-request run, parsed from a real Workflow Result so
    /// the fields under test come from the same grouping the commands use.
    fn run(
        coordinator: &Keys,
        anchor: EventId,
        supplying: EventId,
        created_at: u64,
    ) -> WorkflowRun {
        use nostr::prelude::{EventBuilder, Tag, event::FinalizeEvent};

        let author = coordinator.public_key().to_hex();
        let tag = |values: &[&str]| {
            Tag::parse(
                values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<String>>(),
            )
            .expect("test tag parses")
        };
        let event = EventBuilder::new(kinds::KIND_CI_WORKFLOW_RESULT, "")
            .tags(vec![
                tag(&["a", &format!("30617:{author}:repo")]),
                tag(&["c", &"ab".repeat(20)]),
                tag(&["w", "ci.yml", &"cd".repeat(32)]),
                tag(&["o", "pull_request"]),
                tag(&["E", &anchor.to_hex()]),
                tag(&["K", "1618"]),
                tag(&["P", &author]),
                tag(&["e", &supplying.to_hex()]),
                tag(&["k", "1618"]),
                tag(&["p", &author]),
                tag(&["r", "run"]),
                tag(&["conclusion", "success"]),
            ])
            .custom_created_at(Timestamp::from_secs(created_at))
            .finalize(coordinator)
            .expect("test event finalizes");

        group_workflow_runs(std::slice::from_ref(&event))
            .runs
            .pop()
            .expect("a workflow result groups into one run")
    }

    #[test]
    fn a_pr_target_splits_its_runs_by_revision() {
        let anchor = EventId::from_slice(&[0xab; 32]).expect("valid id");
        let revision = EventId::from_slice(&[0xcd; 32]).expect("valid id");
        let target = Target::PullRequest {
            root: anchor,
            anchor,
            revision,
        };

        let current = run(&Keys::generate(), anchor, revision, 100);
        let earlier = run(&Keys::generate(), anchor, anchor, 90);
        // A run of a PR ngit is not describing is neither.
        let foreign = run(
            &Keys::generate(),
            EventId::from_slice(&[0xef; 32]).expect("valid id"),
            revision,
            100,
        );

        let selection = target.select(&[current.clone(), earlier.clone(), foreign]);
        assert_eq!(selection.current.len(), 1);
        assert_eq!(selection.current[0].coordinator, current.coordinator);
        assert_eq!(selection.outdated.len(), 1);
        assert_eq!(selection.outdated[0].supplying_event, Some(anchor));
        assert!(selection.revision_matched);

        // CI exists for the PR, but only for a superseded revision.
        let selection = target.select(std::slice::from_ref(&earlier));
        assert!(selection.current.is_empty());
        assert!(
            !selection.revision_matched,
            "a result for an earlier revision does not describe the current one"
        );

        // No CI at all is "no CI", not "CI for something else".
        assert!(target.select(&[]).revision_matched);
    }

    #[test]
    fn outdated_attempts_are_collapsed_within_a_revision_not_across_them() {
        let anchor = EventId::from_slice(&[0xab; 32]).expect("valid id");
        let first = EventId::from_slice(&[0x11; 32]).expect("valid id");
        let second = EventId::from_slice(&[0x22; 32]).expect("valid id");
        // One coordinator, one workflow, three attempts across two
        // superseded revisions: collapsing the whole set — which is what
        // `latest_attempts` alone would do — would leave one.
        let coordinator = Keys::generate();
        let attempts = outdated_attempts(&[
            run(&coordinator, anchor, first, 100),
            run(&coordinator, anchor, second, 200),
            run(&coordinator, anchor, second, 210),
        ]);
        assert_eq!(
            attempts.len(),
            2,
            "each revision keeps its own latest attempt, and only its own"
        );
        assert_eq!(
            attempts[0].run.supplying_event,
            Some(second),
            "the newest superseded revision comes first"
        );
        assert_eq!(
            attempts[0].attempt_of, 2,
            "the newer revision's attempt is the second of its revision"
        );
        assert_eq!(attempts[1].run.supplying_event, Some(first));
        assert_eq!(attempts[1].attempt_of, 1);
    }

    #[test]
    fn an_outdated_run_that_named_no_supplying_event_reports_a_null_revision() {
        let anchor = EventId::from_slice(&[0xab; 32]).expect("valid id");
        let mut run = run(&Keys::generate(), anchor, anchor, 100);
        // A PR-context run whose `e` tag named nothing: it describes the
        // root, which is not this PR's current revision.
        run.supplying_event = None;

        let report = CiReport {
            state: CiState::None,
            conclusion: None,
            revision_matched: false,
            coverage: Coverage::Complete,
            rollup: TrustResolution::Settled {
                classification: TrustClassification::NoKnownContext,
                evidence: Vec::new(),
                coverage: Coverage::Complete,
            },
            runs: Vec::new(),
            outdated: Some(vec![RunReport {
                state: run.state(Timestamp::now()),
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
                run,
            }]),
            skipped: Vec::new(),
        };

        assert_eq!(
            report.to_ci_value(None)["outdated"][0]["revision"],
            Value::Null,
            "a run with no supplying event names no revision, rather than \
             borrowing the current one"
        );
    }

    /// A report with no runs of its own: only the fields the gate and the
    /// warning read are meaningful.
    fn rolled_up(
        state: CiState,
        conclusion: Option<Conclusion>,
        classification: TrustClassification,
    ) -> CiReport {
        CiReport {
            state,
            conclusion,
            revision_matched: true,
            coverage: Coverage::Complete,
            rollup: TrustResolution::Settled {
                classification,
                evidence: Vec::new(),
                coverage: Coverage::Complete,
            },
            runs: Vec::new(),
            outdated: None,
            skipped: Vec::new(),
        }
    }

    #[test]
    fn the_merge_warning_is_the_gate_shortfall_at_the_default_floor() {
        let failing = rolled_up(
            CiState::Concluded,
            Some(Conclusion::Failure),
            TrustClassification::MaintainerDirected,
        );
        let stale = rolled_up(
            CiState::Stale,
            None,
            TrustClassification::MaintainerDirected,
        );
        let weak = rolled_up(
            CiState::Concluded,
            Some(Conclusion::Success),
            TrustClassification::NoKnownContext,
        );
        for report in [&failing, &stale, &weak] {
            assert_eq!(
                report.merge_warning(),
                report.shortfall(DEFAULT_TRUST_FLOOR),
                "the warning is the gate's own shortfall, so a merge that \
                 warns is a merge `--require-ci-trust` would refuse"
            );
            assert!(report.merge_warning().is_some());
        }

        let passing = rolled_up(
            CiState::Concluded,
            Some(Conclusion::Success),
            TrustClassification::OperationallyAssociated,
        );
        assert_eq!(passing.merge_warning(), None);
        assert!(
            passing
                .gate_failure(CiTrustFloor::MaintainerDirected)
                .is_some(),
            "the default floor is not the strictest one: a stricter floor \
             still refuses what the warning is silent about"
        );
    }

    #[test]
    fn a_target_with_no_ci_at_all_does_not_warn_on_merge() {
        let none = rolled_up(CiState::None, None, TrustClassification::NoKnownContext);
        assert_eq!(
            none.merge_warning(),
            None,
            "no CI is not a failing, unfinished or weakly-signed result"
        );
        assert!(
            none.gate_failure(DEFAULT_TRUST_FLOOR).is_some(),
            "a caller that demanded a floor is still refused: there is no \
             result to meet it"
        );
    }

    #[test]
    fn ci_for_a_superseded_revision_only_warns_on_merge() {
        let mut superseded = rolled_up(CiState::None, None, TrustClassification::NoKnownContext);
        superseded.revision_matched = false;
        assert!(
            superseded.merge_warning().is_some(),
            "a PR whose only CI describes an earlier revision must not be \
             merged in silence"
        );
    }

    #[test]
    fn an_unsettled_trust_context_fails_the_floor_rather_than_passing_it() {
        let unsettled = CiReport {
            rollup: TrustResolution::Loading,
            ..rolled_up(
                CiState::Concluded,
                Some(Conclusion::Success),
                TrustClassification::MaintainerDirected,
            )
        };
        assert!(
            unsettled.shortfall(DEFAULT_TRUST_FLOOR).is_some(),
            "an absent classification is evidence that has not settled, not \
             evidence that met the floor"
        );
        assert!(unsettled.merge_warning().is_some());
    }

    #[test]
    fn the_gate_and_the_glyph_agree_on_every_conclusion() {
        // Trust is held at the strongest there is, so the only thing either
        // surface can be reacting to is the conclusion itself.
        for conclusion in [
            Conclusion::Success,
            Conclusion::Neutral,
            Conclusion::Skipped,
            Conclusion::Cancelled,
            Conclusion::Failure,
            Conclusion::TimedOut,
            Conclusion::StartupFailure,
        ] {
            let report = rolled_up(
                CiState::Concluded,
                Some(conclusion),
                TrustClassification::MaintainerDirected,
            );
            let green = is_green(conclusion);
            assert_eq!(
                report.shortfall(DEFAULT_TRUST_FLOOR).is_none(),
                green,
                "`{conclusion}` must gate exactly as the one green predicate \
                 says",
            );
            assert_eq!(
                report.merge_warning().is_none(),
                green,
                "`{conclusion}` must warn exactly as it gates",
            );
            assert_eq!(
                row(CiState::Concluded, Some(conclusion), true).glyph() == "✓",
                green,
                "`{conclusion}` must render exactly as it gates: a row that \
                 shows a pass is never a merge the gate refuses",
            );
        }
    }

    #[test]
    fn a_neutral_or_skipped_conclusion_is_green_and_a_cancelled_one_is_not() {
        // The verdicts the agreement above is agreement *on*: a workflow that
        // concluded there was nothing to do does not block a merge, while a
        // cancelled one — nothing ran to completion — does.
        for conclusion in [Conclusion::Neutral, Conclusion::Skipped] {
            let report = rolled_up(
                CiState::Concluded,
                Some(conclusion),
                TrustClassification::OperationallyAssociated,
            );
            assert_eq!(
                report.gate_failure(CiTrustFloor::OperationallyAssociated),
                None,
                "a concluded `{conclusion}` run meets the floor its trust met",
            );
            assert_eq!(report.merge_warning(), None);

            // The trust floor still applies to it: green is about the
            // conclusion, never about who signed it.
            let weak = rolled_up(
                CiState::Concluded,
                Some(conclusion),
                TrustClassification::NoKnownContext,
            );
            assert!(weak.shortfall(DEFAULT_TRUST_FLOOR).is_some());
        }

        let cancelled = rolled_up(
            CiState::Concluded,
            Some(Conclusion::Cancelled),
            TrustClassification::MaintainerDirected,
        );
        assert!(
            cancelled.shortfall(DEFAULT_TRUST_FLOOR).is_some(),
            "nothing ran to completion, so cancelled is not green at any \
             trust level"
        );
    }
}
