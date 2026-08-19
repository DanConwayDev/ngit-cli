//! Kind constants and strict shape validation for the consumed CI events.
//!
//! The ngit-ci kind numbers are experimental placeholders, so this module is
//! the single place they live. Validation is deliberately strict: an event
//! whose shape does not match the NIP is surfaced as a [`ShapeReason`] and
//! skipped by the caller, never reinterpreted as a weaker or different claim.
//!
//! Only the consumed subset is modelled here. Coordinator Advertisement
//! (19843), Request-Readiness (19844), Coordinator Repository Status (39844)
//! and Repository Secret Update (29846) are out of scope.

use std::fmt;

use nostr::prelude::{Event, EventId, Kind, PublicKey, Tag, Timestamp, nip01::Coordinate};

/// Kind 9840 — Manual Trigger, signed by a repository maintainer.
pub const KIND_CI_MANUAL_TRIGGER: Kind = Kind::Custom(9840);
/// Kind 9841 — Job Result, signed by the compute provider.
pub const KIND_CI_JOB_RESULT: Kind = Kind::Custom(9841);
/// Kind 9842 — Workflow Result, signed by the coordinator.
pub const KIND_CI_WORKFLOW_RESULT: Kind = Kind::Custom(9842);
/// Kind 9843 — Service Request, signed by the requester.
pub const KIND_CI_SERVICE_REQUEST: Kind = Kind::Custom(9843);
/// Kind 9844 — Service Stop, signed by the requester.
pub const KIND_CI_SERVICE_STOP: Kind = Kind::Custom(9844);
/// Kind 39842 — Workflow Progress, an expiring addressable run marker.
pub const KIND_CI_WORKFLOW_PROGRESS: Kind = Kind::Custom(39842);

/// Every CI kind ngit consumes.
pub const CONSUMED_CI_KINDS: [Kind; 6] = [
    KIND_CI_MANUAL_TRIGGER,
    KIND_CI_JOB_RESULT,
    KIND_CI_WORKFLOW_RESULT,
    KIND_CI_SERVICE_REQUEST,
    KIND_CI_SERVICE_STOP,
    KIND_CI_WORKFLOW_PROGRESS,
];

/// A Workflow Progress event expires no more than 30 minutes after
/// `created_at`. A longer expiry would let one marker claim "running"
/// indefinitely, so it is rejected rather than clamped.
pub const MAX_PROGRESS_EXPIRATION_SECS: u64 = 30 * 60;

/// Quote marker naming the standing Service Request selected for a run.
pub const MARKER_SERVICE_REQUEST: &str = "service-request";
/// Quote marker naming the Manual Trigger a run replayed.
pub const MARKER_MANUAL_TRIGGER: &str = "manual-trigger";

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// Conclusion values, aligned with the GitHub API `conclusion` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Conclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    StartupFailure,
}

impl Conclusion {
    /// Parse a `conclusion` tag value. Unknown values are rejected rather
    /// than normalized, so a future conclusion is never displayed as one of
    /// today's.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "neutral" => Some(Self::Neutral),
            "cancelled" => Some(Self::Cancelled),
            "skipped" => Some(Self::Skipped),
            "timed_out" => Some(Self::TimedOut),
            "startup_failure" => Some(Self::StartupFailure),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Neutral => "neutral",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::TimedOut => "timed_out",
            Self::StartupFailure => "startup_failure",
        }
    }
}

impl fmt::Display for Conclusion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalized reason an attempt was run (`o` tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    Push,
    PullRequest,
    Schedule,
    Manual,
}

impl Trigger {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "push" => Some(Self::Push),
            "pull_request" => Some(Self::PullRequest),
            "schedule" => Some(Self::Schedule),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::PullRequest => "pull_request",
            Self::Schedule => "schedule",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Workflow Progress `status` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressStatus {
    Queued,
    InProgress,
    Concluded,
}

impl ProgressStatus {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "in_progress" => Some(Self::InProgress),
            "concluded" => Some(Self::Concluded),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Concluded => "concluded",
        }
    }
}

impl fmt::Display for ProgressStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A repository announcement referenced by an `a` tag, with its relay hint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoReference {
    pub coordinate: Coordinate,
    pub relay_hint: Option<String>,
}

/// The workflow file an attempt ran (`w` tag).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowFile {
    pub path: String,
    /// SHA-256 of the workflow file content at the run's commit.
    pub sha256: String,
}

/// NIP-22 pull-request context (`E`/`K`/`P` root, `e`/`k`/`p` parent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestContext {
    /// The kind-1618 pull request the run belongs to.
    pub root: EventId,
    pub root_author: Option<PublicKey>,
    /// The 1618/1619 event that supplied the commit.
    pub supplying_event: Option<EventId>,
    pub supplying_kind: Option<Kind>,
    pub supplying_author: Option<PublicKey>,
}

/// Trigger context: a Git ref for push/tag runs, NIP-22 tags for PR runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerContext {
    Push { git_ref: String },
    PullRequest(PullRequestContext),
}

/// The tags every CI event carries: repository, commit, workflow file,
/// normalized trigger and trigger context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonTags {
    /// Repository announcements the event names. All share one identifier.
    pub repositories: Vec<RepoReference>,
    /// `c` tags: the first is the peeled commit the workflow ran against,
    /// any others are annotated-tag object ids that peel to it.
    pub commits: Vec<String>,
    pub workflow: WorkflowFile,
    /// `o` tag. Absent on a Manual Trigger, which is always `manual`.
    pub trigger: Option<Trigger>,
    pub context: Option<TriggerContext>,
}

impl CommonTags {
    /// The commit the workflow ran against.
    #[must_use]
    pub fn commit(&self) -> &str {
        // parsing guarantees at least one `c` tag
        self.commits.first().map_or("", String::as_str)
    }

    #[must_use]
    pub fn pr_root(&self) -> Option<EventId> {
        match &self.context {
            Some(TriggerContext::PullRequest(pr)) => Some(pr.root),
            _ => None,
        }
    }

    #[must_use]
    pub fn supplying_event(&self) -> Option<EventId> {
        match &self.context {
            Some(TriggerContext::PullRequest(pr)) => pr.supplying_event,
            _ => None,
        }
    }

    #[must_use]
    pub fn git_ref(&self) -> Option<&str> {
        match &self.context {
            Some(TriggerContext::Push { git_ref }) => Some(git_ref.as_str()),
            _ => None,
        }
    }
}

/// Which request a run's frozen provenance quote names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProvenanceKind {
    ServiceRequest,
    ManualTrigger,
}

/// A run's frozen request-provenance quote.
///
/// This layer types the reference only. Fetching the quoted event and
/// checking that it really is a maintainer request is the provenance module's
/// job; a coordinator-authored `q` tag is not evidence on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceQuote {
    pub kind: ProvenanceKind,
    pub event_id: EventId,
    pub relay: Option<String>,
    /// Required pubkey hint: both quoted kinds are regular events.
    pub requester: PublicKey,
}

/// A Workflow Result / Progress quote of one Job Result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobQuote {
    pub event_id: EventId,
    pub relay: Option<String>,
    pub publisher: Option<PublicKey>,
    pub job_id: String,
}

/// The addressable Workflow Progress event a Job Result belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowRunAddress {
    pub coordinator: PublicKey,
    pub run_id: String,
}

// ---------------------------------------------------------------------------
// Validated events
// ---------------------------------------------------------------------------

/// A validated kind-9840 Manual Trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualTrigger {
    pub event_id: EventId,
    pub author: PublicKey,
    pub created_at: Timestamp,
    /// The `p` tagged coordinator. On a Manual Trigger the single `p` slot is
    /// the coordinator address, on a PR-context trigger as much as any other.
    pub coordinator: PublicKey,
    pub common: CommonTags,
}

/// A validated kind-9843 Service Request or kind-9844 Service Stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceControl {
    pub event_id: EventId,
    pub author: PublicKey,
    pub created_at: Timestamp,
    /// The `p` tagged coordinator.
    pub coordinator: PublicKey,
    /// The single repository perspective the control covers.
    pub repository: RepoReference,
    /// `true` for kind 9843, `false` for kind 9844.
    pub is_request: bool,
}

/// A validated kind-9841 Job Result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobResult {
    pub event_id: EventId,
    /// The compute provider that executed the job.
    pub author: PublicKey,
    pub created_at: Timestamp,
    /// The run the job belongs to, from the quoted 39842 address.
    pub run: WorkflowRunAddress,
    pub run_relay_hint: Option<String>,
    pub job_id: String,
    pub name: Option<String>,
    pub conclusion: Conclusion,
    pub queued_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub logs: Option<String>,
    /// A Job Result MAY quote the Manual Trigger, but never a Service
    /// Request: the coordinator made that authorization decision.
    pub provenance: Option<ProvenanceQuote>,
    pub common: CommonTags,
}

/// A validated kind-9842 Workflow Result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowResult {
    pub event_id: EventId,
    /// The coordinator that accepted the run.
    pub author: PublicKey,
    pub created_at: Timestamp,
    pub run_id: String,
    pub conclusion: Conclusion,
    pub queued_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub provenance: Option<ProvenanceQuote>,
    pub jobs: Vec<JobQuote>,
    pub common: CommonTags,
}

/// A validated kind-39842 Workflow Progress marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowProgress {
    pub event_id: EventId,
    pub author: PublicKey,
    pub created_at: Timestamp,
    pub run_id: String,
    pub status: ProgressStatus,
    pub conclusion: Option<Conclusion>,
    pub expiration: Timestamp,
    pub queued_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub queue: Option<u64>,
    pub in_progress_jobs: Vec<String>,
    pub provenance: Option<ProvenanceQuote>,
    pub jobs: Vec<JobQuote>,
    pub common: CommonTags,
}

impl WorkflowProgress {
    /// Whether the marker is still within its NIP-40 expiry at `now`.
    #[must_use]
    pub fn is_live(&self, now: Timestamp) -> bool {
        self.expiration > now
    }

    /// Whether the marker claims the run is queued or executing.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(
            self.status,
            ProgressStatus::Queued | ProgressStatus::InProgress
        )
    }
}

/// Any validated CI event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiEvent {
    ManualTrigger(ManualTrigger),
    ServiceControl(ServiceControl),
    JobResult(JobResult),
    WorkflowResult(WorkflowResult),
    WorkflowProgress(WorkflowProgress),
}

// ---------------------------------------------------------------------------
// Shape errors
// ---------------------------------------------------------------------------

/// Why an event does not match the shape its kind requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeReason {
    /// Not a CI kind ngit consumes.
    UnexpectedKind(Kind),
    /// The kind requires empty content.
    NonEmptyContent,
    MissingTag(&'static str),
    /// A tag the kind allows at most once appears more than once.
    RepeatedTag(&'static str),
    /// A tag the kind forbids is present.
    ForbiddenTag(&'static str),
    InvalidTagValue {
        tag: &'static str,
        value: String,
    },
    /// `a` tags naming different repository identifiers.
    MismatchedRepositories,
    /// Both a Git-ref `r` tag and NIP-22 pull-request tags.
    ConflictingTriggerContext,
    /// The `o` trigger and the trigger-context tags disagree.
    MissingTriggerContext(Trigger),
    /// A `conclusion` is required but absent, or present when the status is
    /// not `concluded`.
    UnexpectedConclusion,
    InvalidQuote(String),
    /// More than one `service-request` / `manual-trigger` quote.
    RepeatedProvenanceQuote,
}

impl fmt::Display for ShapeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedKind(kind) => write!(f, "kind {kind} is not a consumed CI kind"),
            Self::NonEmptyContent => write!(f, "content must be empty"),
            Self::MissingTag(name) => write!(f, "missing `{name}` tag"),
            Self::RepeatedTag(name) => write!(f, "repeated `{name}` tag"),
            Self::ForbiddenTag(name) => write!(f, "unexpected `{name}` tag"),
            Self::InvalidTagValue { tag, value } => {
                write!(f, "invalid `{tag}` tag value `{value}`")
            }
            Self::MismatchedRepositories => {
                write!(f, "`a` tags name different repository identifiers")
            }
            Self::ConflictingTriggerContext => write!(
                f,
                "both a Git-ref `r` tag and pull-request tags are present"
            ),
            Self::MissingTriggerContext(trigger) => {
                write!(f, "no trigger context for an `{trigger}` trigger")
            }
            Self::UnexpectedConclusion => write!(f, "`conclusion` tag does not match the status"),
            Self::InvalidQuote(detail) => write!(f, "invalid `q` tag: {detail}"),
            Self::RepeatedProvenanceQuote => write!(f, "more than one request-provenance quote"),
        }
    }
}

impl std::error::Error for ShapeReason {}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate any consumed CI event against its kind's shape.
///
/// # Errors
///
/// Returns the [`ShapeReason`] the event failed on. Callers surface it as a
/// skipped event rather than falling back to a looser interpretation.
pub fn validate(event: &Event) -> Result<CiEvent, ShapeReason> {
    let kind = event.kind;
    if kind == KIND_CI_MANUAL_TRIGGER {
        Ok(CiEvent::ManualTrigger(validate_manual_trigger(event)?))
    } else if kind == KIND_CI_SERVICE_REQUEST || kind == KIND_CI_SERVICE_STOP {
        Ok(CiEvent::ServiceControl(validate_service_control(event)?))
    } else if kind == KIND_CI_JOB_RESULT {
        Ok(CiEvent::JobResult(validate_job_result(event)?))
    } else if kind == KIND_CI_WORKFLOW_RESULT {
        Ok(CiEvent::WorkflowResult(validate_workflow_result(event)?))
    } else if kind == KIND_CI_WORKFLOW_PROGRESS {
        Ok(CiEvent::WorkflowProgress(validate_workflow_progress(
            event,
        )?))
    } else {
        Err(ShapeReason::UnexpectedKind(kind))
    }
}

/// Validate a kind-9840 Manual Trigger.
///
/// # Errors
///
/// Returns the [`ShapeReason`] the event failed on.
pub fn validate_manual_trigger(event: &Event) -> Result<ManualTrigger, ShapeReason> {
    require_kind(event, KIND_CI_MANUAL_TRIGGER)?;
    require_empty_content(event)?;
    let view = TagView::new(event);

    let common = parse_common_tags(&view, CommonProfile::ManualTrigger)?;
    // `required_tag` enforces the NIP's "exactly one `p`, which MUST name the
    // coordinator". A PR-context trigger is no exception: the NIP-22 context
    // it carries excludes the participant `p`, so a second `p` cannot be a
    // parent author and tolerating one would let a coordinator the maintainer
    // never addressed pass provenance validation.
    let coordinator = parse_public_key("p", view.required_value("p")?)?;

    Ok(ManualTrigger {
        event_id: event.id,
        author: event.pubkey,
        created_at: event.created_at,
        coordinator,
        common,
    })
}

/// Validate a kind-9843 Service Request or kind-9844 Service Stop.
///
/// # Errors
///
/// Returns the [`ShapeReason`] the event failed on.
pub fn validate_service_control(event: &Event) -> Result<ServiceControl, ShapeReason> {
    let is_request = if event.kind == KIND_CI_SERVICE_REQUEST {
        true
    } else if event.kind == KIND_CI_SERVICE_STOP {
        false
    } else {
        return Err(ShapeReason::UnexpectedKind(event.kind));
    };
    require_empty_content(event)?;
    let view = TagView::new(event);
    view.forbid("d")?;
    view.forbid("expiration")?;

    // `required_tag` enforces the NIP's "exactly one" for both `a` and `p`.
    let repository = parse_repository(view.required_tag("a")?)?;
    let coordinator = parse_public_key("p", view.required_value("p")?)?;

    Ok(ServiceControl {
        event_id: event.id,
        author: event.pubkey,
        created_at: event.created_at,
        coordinator,
        repository,
        is_request,
    })
}

/// Validate a kind-9841 Job Result.
///
/// # Errors
///
/// Returns the [`ShapeReason`] the event failed on.
pub fn validate_job_result(event: &Event) -> Result<JobResult, ShapeReason> {
    require_kind(event, KIND_CI_JOB_RESULT)?;
    let view = TagView::new(event);
    // The association to a run is the quoted 39842 address, never a `d` tag.
    view.forbid("d")?;

    let common = parse_common_tags(&view, CommonProfile::ResultLike)?;
    let quotes = parse_quotes(&view)?;
    // The coordinator made the authorization decision; the provider asserts
    // only the job execution.
    if let Some(provenance) = &quotes.provenance {
        if provenance.kind == ProvenanceKind::ServiceRequest {
            return Err(ShapeReason::InvalidQuote(
                "a Job Result must not quote a Service Request".to_owned(),
            ));
        }
    }
    // Other quotes are not defined for this kind and are ignored rather than
    // rejected. The run association must stay unambiguous, so exactly one
    // Workflow Progress address is required.
    let mut addresses = quotes.addresses.into_iter().filter(|(coordinate, _)| {
        coordinate.kind == KIND_CI_WORKFLOW_PROGRESS && !coordinate.identifier.is_empty()
    });
    let (coordinate, run_relay_hint) = addresses
        .next()
        .ok_or_else(|| ShapeReason::InvalidQuote("no Workflow Progress address".to_owned()))?;
    if addresses.next().is_some() {
        return Err(ShapeReason::InvalidQuote(
            "more than one Workflow Progress address".to_owned(),
        ));
    }

    Ok(JobResult {
        event_id: event.id,
        author: event.pubkey,
        created_at: event.created_at,
        run: WorkflowRunAddress {
            coordinator: coordinate.public_key,
            run_id: coordinate.identifier,
        },
        run_relay_hint,
        job_id: view.required_value("job")?.to_owned(),
        name: view.optional_value("name")?.map(ToOwned::to_owned),
        conclusion: parse_conclusion(view.required_value("conclusion")?)?,
        queued_at: view
            .optional_value("queued_at")?
            .map(|value| parse_timestamp("queued_at", value))
            .transpose()?,
        started_at: view
            .optional_value("started_at")?
            .map(|value| parse_timestamp("started_at", value))
            .transpose()?,
        logs: view.optional_value("logs")?.map(ToOwned::to_owned),
        provenance: quotes.provenance,
        common,
    })
}

/// Validate a kind-9842 Workflow Result.
///
/// # Errors
///
/// Returns the [`ShapeReason`] the event failed on.
pub fn validate_workflow_result(event: &Event) -> Result<WorkflowResult, ShapeReason> {
    require_kind(event, KIND_CI_WORKFLOW_RESULT)?;
    require_empty_content(event)?;
    let view = TagView::new(event);

    let common = parse_common_tags(&view, CommonProfile::ResultLike)?;
    let quotes = parse_quotes(&view)?;

    // On a push-triggered result the `refs/` values are Git refs; the other
    // `r` value is the workflow run id.
    let mut run_ids = view
        .values("r")
        .into_iter()
        .filter(|value| !value.starts_with("refs/"));
    let run_id = run_ids.next().ok_or(ShapeReason::MissingTag("r"))?;
    if run_ids.next().is_some() {
        return Err(ShapeReason::RepeatedTag("r"));
    }

    Ok(WorkflowResult {
        event_id: event.id,
        author: event.pubkey,
        created_at: event.created_at,
        run_id: run_id.to_owned(),
        conclusion: parse_conclusion(view.required_value("conclusion")?)?,
        queued_at: view
            .optional_value("queued_at")?
            .map(|value| parse_timestamp("queued_at", value))
            .transpose()?,
        started_at: view
            .optional_value("started_at")?
            .map(|value| parse_timestamp("started_at", value))
            .transpose()?,
        provenance: quotes.provenance,
        jobs: quotes.jobs,
        common,
    })
}

/// Validate a kind-39842 Workflow Progress marker.
///
/// # Errors
///
/// Returns the [`ShapeReason`] the event failed on.
pub fn validate_workflow_progress(event: &Event) -> Result<WorkflowProgress, ShapeReason> {
    require_kind(event, KIND_CI_WORKFLOW_PROGRESS)?;
    require_empty_content(event)?;
    let view = TagView::new(event);

    let common = parse_common_tags(&view, CommonProfile::ResultLike)?;
    let quotes = parse_quotes(&view)?;

    // `d` replaces the Workflow Result's workflow-run `r`, so any non-ref `r`
    // value carries no meaning here. The NIP does not forbid one, so it is
    // ignored rather than rejected.
    let status = ProgressStatus::parse(view.required_value("status")?).ok_or_else(|| {
        ShapeReason::InvalidTagValue {
            tag: "status",
            value: view.required_value("status").unwrap_or_default().to_owned(),
        }
    })?;
    let conclusion = view
        .optional_value("conclusion")?
        .map(parse_conclusion)
        .transpose()?;
    if conclusion.is_some() != (status == ProgressStatus::Concluded) {
        return Err(ShapeReason::UnexpectedConclusion);
    }
    // Final authorization is only selected at runner handoff, so a queued
    // marker cannot yet carry the frozen service-request quote.
    if status == ProgressStatus::Queued
        && quotes
            .provenance
            .as_ref()
            .is_some_and(|quote| quote.kind == ProvenanceKind::ServiceRequest)
    {
        return Err(ShapeReason::InvalidQuote(
            "a queued Workflow Progress must omit the service-request quote".to_owned(),
        ));
    }

    let expiration = parse_timestamp("expiration", view.required_value("expiration")?)?;
    if expiration <= event.created_at
        || expiration.as_secs() > event.created_at.as_secs() + MAX_PROGRESS_EXPIRATION_SECS
    {
        return Err(ShapeReason::InvalidTagValue {
            tag: "expiration",
            value: expiration.to_string(),
        });
    }

    Ok(WorkflowProgress {
        event_id: event.id,
        author: event.pubkey,
        created_at: event.created_at,
        run_id: view.required_value("d")?.to_owned(),
        status,
        conclusion,
        expiration,
        queued_at: view
            .optional_value("queued_at")?
            .map(|value| parse_timestamp("queued_at", value))
            .transpose()?,
        started_at: view
            .optional_value("started_at")?
            .map(|value| parse_timestamp("started_at", value))
            .transpose()?,
        queue: view
            .optional_value("queue")?
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| ShapeReason::InvalidTagValue {
                        tag: "queue",
                        value: value.to_owned(),
                    })
            })
            .transpose()?,
        in_progress_jobs: view
            .matching("in-progress")
            .flat_map(|tag| tag.iter().skip(1).map(ToOwned::to_owned))
            .collect(),
        provenance: quotes.provenance,
        jobs: quotes.jobs,
        common,
    })
}

// ---------------------------------------------------------------------------
// Tag parsing helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommonProfile {
    /// A Manual Trigger carries the common tags except `o`.
    ManualTrigger,
    /// Job Result, Workflow Result and Workflow Progress.
    ResultLike,
}

struct TagView<'a> {
    tags: Vec<&'a [String]>,
}

impl<'a> TagView<'a> {
    fn new(event: &'a Event) -> Self {
        Self {
            tags: event.tags.iter().map(Tag::as_slice).collect(),
        }
    }

    fn matching(&self, name: &'static str) -> impl Iterator<Item = &'a [String]> + '_ {
        self.tags
            .iter()
            .copied()
            .filter(move |tag| tag.first().is_some_and(|first| first == name))
    }

    fn count(&self, name: &'static str) -> usize {
        self.matching(name).count()
    }

    fn values(&self, name: &'static str) -> Vec<&'a str> {
        self.matching(name)
            .filter_map(|tag| tag.get(1).map(String::as_str))
            .collect()
    }

    /// The first non-empty value for `name`, for tags the NIP may repeat.
    fn first_value(&self, name: &'static str) -> Option<&'a str> {
        self.values(name)
            .into_iter()
            .find(|value| !value.is_empty())
    }

    fn forbid(&self, name: &'static str) -> Result<(), ShapeReason> {
        if self.count(name) > 0 {
            return Err(ShapeReason::ForbiddenTag(name));
        }
        Ok(())
    }

    fn optional_tag(&self, name: &'static str) -> Result<Option<&'a [String]>, ShapeReason> {
        let mut matching = self.matching(name);
        let first = matching.next();
        if matching.next().is_some() {
            return Err(ShapeReason::RepeatedTag(name));
        }
        Ok(first)
    }

    fn required_tag(&self, name: &'static str) -> Result<&'a [String], ShapeReason> {
        self.optional_tag(name)?
            .ok_or(ShapeReason::MissingTag(name))
    }

    fn optional_value(&self, name: &'static str) -> Result<Option<&'a str>, ShapeReason> {
        self.optional_tag(name)?
            .map(|tag| value(name, tag))
            .transpose()
    }

    fn required_value(&self, name: &'static str) -> Result<&'a str, ShapeReason> {
        value(name, self.required_tag(name)?)
    }
}

fn value<'a>(name: &'static str, tag: &'a [String]) -> Result<&'a str, ShapeReason> {
    match tag.get(1) {
        Some(value) if !value.is_empty() => Ok(value.as_str()),
        other => Err(ShapeReason::InvalidTagValue {
            tag: name,
            value: other.cloned().unwrap_or_default(),
        }),
    }
}

fn require_kind(event: &Event, kind: Kind) -> Result<(), ShapeReason> {
    if event.kind == kind {
        Ok(())
    } else {
        Err(ShapeReason::UnexpectedKind(event.kind))
    }
}

fn require_empty_content(event: &Event) -> Result<(), ShapeReason> {
    if event.content.is_empty() {
        Ok(())
    } else {
        Err(ShapeReason::NonEmptyContent)
    }
}

fn parse_timestamp(tag: &'static str, raw: &str) -> Result<Timestamp, ShapeReason> {
    raw.parse::<u64>()
        .map(Timestamp::from_secs)
        .map_err(|_| ShapeReason::InvalidTagValue {
            tag,
            value: raw.to_owned(),
        })
}

fn parse_conclusion(raw: &str) -> Result<Conclusion, ShapeReason> {
    Conclusion::parse(raw).ok_or_else(|| ShapeReason::InvalidTagValue {
        tag: "conclusion",
        value: raw.to_owned(),
    })
}

fn parse_public_key(tag: &'static str, raw: &str) -> Result<PublicKey, ShapeReason> {
    PublicKey::parse(raw).map_err(|_| ShapeReason::InvalidTagValue {
        tag,
        value: raw.to_owned(),
    })
}

fn parse_event_id(tag: &'static str, raw: &str) -> Result<EventId, ShapeReason> {
    EventId::parse(raw).map_err(|_| ShapeReason::InvalidTagValue {
        tag,
        value: raw.to_owned(),
    })
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_repository(tag: &[String]) -> Result<RepoReference, ShapeReason> {
    let raw = value("a", tag)?;
    let coordinate =
        Coordinate::from_kpi_format(raw).map_err(|_| ShapeReason::InvalidTagValue {
            tag: "a",
            value: raw.to_owned(),
        })?;
    if coordinate.kind != Kind::GitRepoAnnouncement || coordinate.identifier.is_empty() {
        return Err(ShapeReason::InvalidTagValue {
            tag: "a",
            value: raw.to_owned(),
        });
    }
    Ok(RepoReference {
        coordinate,
        relay_hint: tag.get(2).filter(|hint| !hint.is_empty()).cloned(),
    })
}

fn parse_common_tags(
    view: &TagView<'_>,
    profile: CommonProfile,
) -> Result<CommonTags, ShapeReason> {
    let repositories = view
        .matching("a")
        .map(parse_repository)
        .collect::<Result<Vec<_>, _>>()?;
    let first = repositories.first().ok_or(ShapeReason::MissingTag("a"))?;
    if repositories
        .iter()
        .any(|repo| repo.coordinate.identifier != first.coordinate.identifier)
    {
        return Err(ShapeReason::MismatchedRepositories);
    }

    let commits: Vec<String> = view.values("c").iter().map(|c| (*c).to_owned()).collect();
    if commits.is_empty() {
        return Err(ShapeReason::MissingTag("c"));
    }
    if let Some(invalid) = commits.iter().find(|commit| !is_object_id(commit)) {
        return Err(ShapeReason::InvalidTagValue {
            tag: "c",
            value: invalid.clone(),
        });
    }

    let workflow_tag = view.required_tag("w")?;
    let path = value("w", workflow_tag)?.to_owned();
    let sha256 = workflow_tag
        .get(2)
        .filter(|hash| hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| ShapeReason::InvalidTagValue {
            tag: "w",
            value: workflow_tag.get(2).cloned().unwrap_or_default(),
        })?
        .clone();

    let trigger = match profile {
        // A Manual Trigger is always normalized as `manual` and omits `o`.
        CommonProfile::ManualTrigger => {
            view.forbid("o")?;
            None
        }
        CommonProfile::ResultLike => {
            let raw = view.required_value("o")?;
            Some(
                Trigger::parse(raw).ok_or_else(|| ShapeReason::InvalidTagValue {
                    tag: "o",
                    value: raw.to_owned(),
                })?,
            )
        }
    };

    let context = parse_trigger_context(view, profile)?;
    match (trigger, &context) {
        (Some(Trigger::Push), Some(TriggerContext::Push { .. }))
        | (Some(Trigger::PullRequest), Some(TriggerContext::PullRequest(_))) => {}
        (Some(trigger @ (Trigger::Push | Trigger::PullRequest)), _) => {
            return Err(ShapeReason::MissingTriggerContext(trigger));
        }
        _ => {}
    }

    Ok(CommonTags {
        repositories,
        commits,
        workflow: WorkflowFile { path, sha256 },
        trigger,
        context,
    })
}

fn parse_trigger_context(
    view: &TagView<'_>,
    profile: CommonProfile,
) -> Result<Option<TriggerContext>, ShapeReason> {
    let git_refs: Vec<&str> = view
        .values("r")
        .into_iter()
        .filter(|value| value.starts_with("refs/"))
        .collect();
    let root = view.optional_value("E")?;

    if !git_refs.is_empty() && root.is_some() {
        return Err(ShapeReason::ConflictingTriggerContext);
    }
    if git_refs.len() > 1 {
        return Err(ShapeReason::RepeatedTag("r"));
    }
    if let Some(git_ref) = git_refs.first() {
        return Ok(Some(TriggerContext::Push {
            git_ref: (*git_ref).to_owned(),
        }));
    }
    let Some(root) = root else {
        return Ok(None);
    };

    // A Manual Trigger's single `p` names the coordinator: the NIP-22 context
    // it carries excludes the participant `p`, so a PR-context trigger states
    // no supplying author. Result-like events carry exclusively NIP-22 `p`
    // tags, and the first is used: the NIP places no upper bound on them.
    let supplying_author = match profile {
        CommonProfile::ManualTrigger => None,
        CommonProfile::ResultLike => view
            .first_value("p")
            .map(|raw| parse_public_key("p", raw))
            .transpose()?,
    };

    Ok(Some(TriggerContext::PullRequest(PullRequestContext {
        root: parse_event_id("E", root)?,
        root_author: view
            .optional_value("P")?
            .map(|raw| parse_public_key("P", raw))
            .transpose()?,
        supplying_event: view
            .optional_value("e")?
            .map(|raw| parse_event_id("e", raw))
            .transpose()?,
        supplying_kind: view
            .optional_value("k")?
            .map(|raw| {
                raw.parse::<u16>()
                    .map(Kind::from_u16)
                    .map_err(|_| ShapeReason::InvalidTagValue {
                        tag: "k",
                        value: raw.to_owned(),
                    })
            })
            .transpose()?,
        supplying_author,
    })))
}

#[derive(Default)]
struct Quotes {
    provenance: Option<ProvenanceQuote>,
    jobs: Vec<JobQuote>,
    /// Quoted addressable coordinates, e.g. a Job Result's 39842 address.
    addresses: Vec<(Coordinate, Option<String>)>,
}

fn parse_quotes(view: &TagView<'_>) -> Result<Quotes, ShapeReason> {
    let mut quotes = Quotes::default();
    for tag in view.matching("q") {
        let raw = value("q", tag)?;
        let relay = tag.get(2).filter(|hint| !hint.is_empty()).cloned();
        let marker = tag.get(4).filter(|marker| !marker.is_empty());

        if let Ok(coordinate) = Coordinate::from_kpi_format(raw) {
            quotes.addresses.push((coordinate, relay));
            continue;
        }
        let event_id = parse_event_id("q", raw)?;
        // A quote with no marker is not one this NIP defines; ignore it
        // rather than reinterpreting it as a job or as provenance.
        let Some(marker) = marker else {
            continue;
        };
        let publisher = tag
            .get(3)
            .filter(|hint| !hint.is_empty())
            .map(|raw| parse_public_key("q", raw))
            .transpose()?;

        let provenance_kind = match marker.as_str() {
            MARKER_SERVICE_REQUEST => Some(ProvenanceKind::ServiceRequest),
            MARKER_MANUAL_TRIGGER => Some(ProvenanceKind::ManualTrigger),
            _ => None,
        };
        if let Some(kind) = provenance_kind {
            // Both quoted kinds are regular events, so the requester hint is
            // required to find them again.
            let requester = publisher.ok_or_else(|| {
                ShapeReason::InvalidQuote(format!("`{marker}` quote without a requester pubkey"))
            })?;
            if quotes.provenance.is_some() {
                return Err(ShapeReason::RepeatedProvenanceQuote);
            }
            quotes.provenance = Some(ProvenanceQuote {
                kind,
                event_id,
                relay,
                requester,
            });
        } else {
            quotes.jobs.push(JobQuote {
                event_id,
                relay,
                publisher,
                job_id: marker.clone(),
            });
        }
    }
    Ok(quotes)
}

#[cfg(test)]
pub(crate) mod test_events {
    //! Event builders shared by the CI unit tests.

    use nostr::prelude::{Event, EventBuilder, Keys, Kind, Tag, Timestamp, event::FinalizeEvent};

    use super::*;

    pub const COMMIT: &str = "1111111111111111111111111111111111111111";
    pub const WORKFLOW_PATH: &str = ".ngit/act/workflows/ci.yml";
    pub const WORKFLOW_HASH: &str =
        "2222222222222222222222222222222222222222222222222222222222222222";

    pub fn tag(values: &[&str]) -> Tag {
        Tag::parse(values.iter().map(ToString::to_string).collect::<Vec<_>>()).unwrap()
    }

    pub fn repo_coordinate(owner: &Keys) -> String {
        format!(
            "{}:{}:ngit",
            Kind::GitRepoAnnouncement.as_u16(),
            owner.public_key().to_hex()
        )
    }

    pub fn common_tags(owner: &Keys) -> Vec<Tag> {
        vec![
            tag(&["a", &repo_coordinate(owner)]),
            tag(&["c", COMMIT]),
            tag(&["w", WORKFLOW_PATH, WORKFLOW_HASH]),
            tag(&["o", "push"]),
            tag(&["r", "refs/heads/main"]),
        ]
    }

    pub fn event(keys: &Keys, kind: Kind, created_at: u64, tags: Vec<Tag>) -> Event {
        EventBuilder::new(kind, "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(created_at))
            .finalize(keys)
            .unwrap()
    }

    /// A minimal valid Workflow Result.
    pub fn workflow_result(
        coordinator: &Keys,
        owner: &Keys,
        run_id: &str,
        created_at: u64,
    ) -> Event {
        let mut tags = common_tags(owner);
        tags.extend([
            tag(&["r", run_id]),
            tag(&["conclusion", "success"]),
            tag(&["started_at", &created_at.to_string()]),
        ]);
        event(coordinator, KIND_CI_WORKFLOW_RESULT, created_at, tags)
    }

    /// A minimal valid Workflow Progress marker.
    pub fn workflow_progress(
        coordinator: &Keys,
        owner: &Keys,
        run_id: &str,
        status: ProgressStatus,
        created_at: u64,
        expiration: u64,
    ) -> Event {
        let mut tags = common_tags(owner);
        tags.extend([
            tag(&["d", run_id]),
            tag(&["status", status.as_str()]),
            tag(&["expiration", &expiration.to_string()]),
        ]);
        if status == ProgressStatus::Concluded {
            tags.push(tag(&["conclusion", "success"]));
        }
        event(coordinator, KIND_CI_WORKFLOW_PROGRESS, created_at, tags)
    }

    /// A minimal valid Job Result quoting `run_id` under `coordinator`.
    pub fn job_result(
        provider: &Keys,
        coordinator: &Keys,
        owner: &Keys,
        run_id: &str,
        job_id: &str,
        created_at: u64,
    ) -> Event {
        let mut tags = common_tags(owner);
        tags.extend([
            tag(&[
                "q",
                &format!(
                    "{}:{}:{run_id}",
                    KIND_CI_WORKFLOW_PROGRESS.as_u16(),
                    coordinator.public_key().to_hex()
                ),
                "wss://relay.example",
            ]),
            tag(&["job", job_id]),
            tag(&["conclusion", "success"]),
        ]);
        event(provider, KIND_CI_JOB_RESULT, created_at, tags)
    }

    /// A minimal valid Service Request (9843) or Service Stop (9844).
    pub fn service_control(
        author: &Keys,
        coordinator: &Keys,
        owner: &Keys,
        is_request: bool,
        created_at: u64,
    ) -> Event {
        event(
            author,
            if is_request {
                KIND_CI_SERVICE_REQUEST
            } else {
                KIND_CI_SERVICE_STOP
            },
            created_at,
            vec![
                tag(&["a", &repo_coordinate(owner)]),
                tag(&["p", &coordinator.public_key().to_hex()]),
            ],
        )
    }

    /// A minimal valid Manual Trigger.
    pub fn manual_trigger(
        author: &Keys,
        coordinator: &Keys,
        owner: &Keys,
        created_at: u64,
    ) -> Event {
        event(
            author,
            KIND_CI_MANUAL_TRIGGER,
            created_at,
            vec![
                tag(&["p", &coordinator.public_key().to_hex()]),
                tag(&["a", &repo_coordinate(owner)]),
                tag(&["c", COMMIT]),
                tag(&["w", WORKFLOW_PATH, WORKFLOW_HASH]),
                tag(&["r", "refs/heads/main"]),
            ],
        )
    }

    /// Rebuild `event` with `tags` replaced by `f`.
    pub fn with_tags(keys: &Keys, event: &Event, f: impl FnOnce(&mut Vec<Tag>)) -> Event {
        let mut tags: Vec<Tag> = event.tags.iter().cloned().collect();
        f(&mut tags);
        EventBuilder::new(event.kind, event.content.clone())
            .tags(tags)
            .custom_created_at(event.created_at)
            .finalize(keys)
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, event::FinalizeEvent};

    use super::{test_events::*, *};

    #[test]
    fn workflow_result_parses_run_id_commit_and_workflow() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let parsed =
            validate_workflow_result(&workflow_result(&coordinator, &owner, "run-1", 100)).unwrap();
        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.conclusion, Conclusion::Success);
        assert_eq!(parsed.common.commit(), COMMIT);
        assert_eq!(parsed.common.workflow.path, WORKFLOW_PATH);
        assert_eq!(parsed.common.trigger, Some(Trigger::Push));
        assert_eq!(parsed.common.git_ref(), Some("refs/heads/main"));
    }

    #[test]
    fn workflow_result_rejects_non_empty_content() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = EventBuilder::new(base.kind, "log output")
            .tags(base.tags.iter().cloned().collect::<Vec<_>>())
            .finalize(&coordinator)
            .unwrap();
        assert_eq!(
            validate_workflow_result(&event),
            Err(ShapeReason::NonEmptyContent)
        );
    }

    #[test]
    fn workflow_result_rejects_missing_run_id() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().get(1).map(String::as_str) != Some("run-1"));
        });
        assert_eq!(
            validate_workflow_result(&event),
            Err(ShapeReason::MissingTag("r"))
        );
    }

    #[test]
    fn workflow_result_rejects_unknown_conclusion() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("conclusion"));
            tags.push(tag(&["conclusion", "exploded"]));
        });
        assert_eq!(
            validate_workflow_result(&event),
            Err(ShapeReason::InvalidTagValue {
                tag: "conclusion",
                value: "exploded".to_owned(),
            })
        );
    }

    #[test]
    fn workflow_result_ignores_quotes_this_nip_does_not_define() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let provider = Keys::generate();
        let job = job_result(&provider, &coordinator, &owner, "run-1", "build", 100);
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            // An unmarked event quote and an address quote: neither is a job
            // quote, and neither invalidates the result.
            tags.push(tag(&["q", &job.id.to_hex(), ""]));
            tags.push(tag(&[
                "q",
                &format!(
                    "{}:{}:run-1",
                    KIND_CI_WORKFLOW_PROGRESS.as_u16(),
                    coordinator.public_key().to_hex()
                ),
            ]));
        });
        let parsed = validate_workflow_result(&event).unwrap();
        assert!(parsed.jobs.is_empty());
        assert!(parsed.provenance.is_none());
    }

    #[test]
    fn result_like_events_accept_a_second_pull_request_p_tag() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let pr_root = Keys::generate();
        let pr_author = Keys::generate();
        let other = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            // Drop the push context, keeping the workflow-run `r`.
            tags.retain(|tag| {
                let slice = tag.as_slice();
                slice.first().map(String::as_str) != Some("o")
                    && slice.get(1).map(String::as_str) != Some("refs/heads/main")
            });
            tags.extend([
                tag(&["o", "pull_request"]),
                tag(&["E", &pr_root.public_key().to_hex()]),
                tag(&["p", &pr_author.public_key().to_hex()]),
                tag(&["p", &other.public_key().to_hex()]),
            ]);
        });
        let parsed = validate_workflow_result(&event).unwrap();
        let TriggerContext::PullRequest(pr) = parsed.common.context.unwrap() else {
            panic!("expected a pull-request context");
        };
        assert_eq!(pr.supplying_author, Some(pr_author.public_key()));
    }

    #[test]
    fn workflow_result_rejects_provenance_quote_without_requester() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let request = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&[
                "q",
                &request.public_key().to_hex(),
                "wss://relay.example",
                "",
                MARKER_SERVICE_REQUEST,
            ]));
        });
        assert!(matches!(
            validate_workflow_result(&event),
            Err(ShapeReason::InvalidQuote(_))
        ));
    }

    #[test]
    fn workflow_result_rejects_pull_request_trigger_without_pr_tags() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("o"));
            tags.push(tag(&["o", "pull_request"]));
        });
        assert_eq!(
            validate_workflow_result(&event),
            Err(ShapeReason::MissingTriggerContext(Trigger::PullRequest))
        );
    }

    #[test]
    fn workflow_result_rejects_git_ref_and_pull_request_tags_together() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let pr_root = Keys::generate().public_key().to_hex();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&["E", &pr_root]));
        });
        assert_eq!(
            validate_workflow_result(&event),
            Err(ShapeReason::ConflictingTriggerContext)
        );
    }

    #[test]
    fn workflow_progress_requires_conclusion_only_when_concluded() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::InProgress,
            100,
            700,
        );
        assert!(validate_workflow_progress(&base).is_ok());

        let event = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&["conclusion", "success"]));
        });
        assert_eq!(
            validate_workflow_progress(&event),
            Err(ShapeReason::UnexpectedConclusion)
        );
    }

    #[test]
    fn workflow_progress_rejects_expiration_beyond_thirty_minutes() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let event = workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::Queued,
            100,
            100 + MAX_PROGRESS_EXPIRATION_SECS + 1,
        );
        assert!(matches!(
            validate_workflow_progress(&event),
            Err(ShapeReason::InvalidTagValue {
                tag: "expiration",
                ..
            })
        ));
    }

    #[test]
    fn workflow_progress_rejects_queued_service_request_quote() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let request = service_control(&maintainer, &coordinator, &owner, true, 50);
        let base = workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::Queued,
            100,
            700,
        );
        let event = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&[
                "q",
                &request.id.to_hex(),
                "wss://relay.example",
                &maintainer.public_key().to_hex(),
                MARKER_SERVICE_REQUEST,
            ]));
        });
        assert!(matches!(
            validate_workflow_progress(&event),
            Err(ShapeReason::InvalidQuote(_))
        ));
    }

    #[test]
    fn workflow_progress_ignores_a_non_ref_r_tag() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_progress(
            &coordinator,
            &owner,
            "run-1",
            ProgressStatus::Queued,
            100,
            700,
        );
        let event = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&["r", "run-1"]));
        });
        let parsed = validate_workflow_progress(&event).unwrap();
        assert_eq!(parsed.run_id, "run-1", "the run id comes from `d`");
    }

    #[test]
    fn job_result_ignores_quotes_this_nip_does_not_define() {
        let provider = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = job_result(&provider, &coordinator, &owner, "run-1", "build", 100);
        let other = job_result(&provider, &coordinator, &owner, "run-1", "test", 100);
        let event = with_tags(&provider, &base, |tags| {
            tags.push(tag(&["q", &other.id.to_hex(), "", "", "test"]));
            tags.push(tag(&["q", &other.id.to_hex(), ""]));
        });
        let parsed = validate_job_result(&event).unwrap();
        assert_eq!(parsed.run.run_id, "run-1");
        assert_eq!(parsed.job_id, "build");
    }

    #[test]
    fn job_result_parses_run_address_and_job() {
        let provider = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let parsed = validate_job_result(&job_result(
            &provider,
            &coordinator,
            &owner,
            "run-1",
            "build",
            100,
        ))
        .unwrap();
        assert_eq!(parsed.run.coordinator, coordinator.public_key());
        assert_eq!(parsed.run.run_id, "run-1");
        assert_eq!(parsed.job_id, "build");
        assert_eq!(parsed.author, provider.public_key());
    }

    #[test]
    fn job_result_rejects_service_request_quote() {
        let provider = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let request = service_control(&maintainer, &coordinator, &owner, true, 50);
        let base = job_result(&provider, &coordinator, &owner, "run-1", "build", 100);
        let event = with_tags(&provider, &base, |tags| {
            tags.push(tag(&[
                "q",
                &request.id.to_hex(),
                "wss://relay.example",
                &maintainer.public_key().to_hex(),
                MARKER_SERVICE_REQUEST,
            ]));
        });
        assert!(matches!(
            validate_job_result(&event),
            Err(ShapeReason::InvalidQuote(_))
        ));
    }

    #[test]
    fn job_result_rejects_d_tag_association() {
        let provider = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = job_result(&provider, &coordinator, &owner, "run-1", "build", 100);
        let event = with_tags(&provider, &base, |tags| {
            tags.push(tag(&["d", "run-1"]));
        });
        assert_eq!(
            validate_job_result(&event),
            Err(ShapeReason::ForbiddenTag("d"))
        );
    }

    #[test]
    fn service_control_parses_perspective_and_coordinator() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let parsed = validate_service_control(&service_control(
            &maintainer,
            &coordinator,
            &owner,
            true,
            10,
        ))
        .unwrap();
        assert!(parsed.is_request);
        assert_eq!(parsed.author, maintainer.public_key());
        assert_eq!(parsed.coordinator, coordinator.public_key());
        assert_eq!(parsed.repository.coordinate.public_key, owner.public_key());

        let stop = validate_service_control(&service_control(
            &maintainer,
            &coordinator,
            &owner,
            false,
            11,
        ))
        .unwrap();
        assert!(!stop.is_request);
    }

    #[test]
    fn service_control_rejects_expiration_and_extra_addresses() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = service_control(&maintainer, &coordinator, &owner, true, 10);

        let expiring = with_tags(&maintainer, &base, |tags| {
            tags.push(tag(&["expiration", "99999"]));
        });
        assert_eq!(
            validate_service_control(&expiring),
            Err(ShapeReason::ForbiddenTag("expiration"))
        );

        let two_repos = with_tags(&maintainer, &base, |tags| {
            tags.push(tag(&["a", &repo_coordinate(&Keys::generate())]));
        });
        assert_eq!(
            validate_service_control(&two_repos),
            Err(ShapeReason::RepeatedTag("a"))
        );
    }

    #[test]
    fn service_control_rejects_non_empty_content() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = service_control(&maintainer, &coordinator, &owner, true, 10);
        let event = EventBuilder::new(base.kind, "please")
            .tags(base.tags.iter().cloned().collect::<Vec<_>>())
            .finalize(&maintainer)
            .unwrap();
        assert_eq!(
            validate_service_control(&event),
            Err(ShapeReason::NonEmptyContent)
        );
    }

    #[test]
    fn manual_trigger_rejects_normalized_trigger_tag() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = manual_trigger(&maintainer, &coordinator, &owner, 10);
        assert!(validate_manual_trigger(&base).is_ok());

        let event = with_tags(&maintainer, &base, |tags| {
            tags.push(tag(&["o", "manual"]));
        });
        assert_eq!(
            validate_manual_trigger(&event),
            Err(ShapeReason::ForbiddenTag("o"))
        );
    }

    #[test]
    fn manual_trigger_rejects_git_ref_with_pull_request_tags() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = manual_trigger(&maintainer, &coordinator, &owner, 10);
        let event = with_tags(&maintainer, &base, |tags| {
            tags.push(tag(&["E", &Keys::generate().public_key().to_hex()]));
        });
        assert_eq!(
            validate_manual_trigger(&event),
            Err(ShapeReason::ConflictingTriggerContext)
        );
    }

    #[test]
    fn manual_trigger_addresses_exactly_one_coordinator() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = manual_trigger(&maintainer, &coordinator, &owner, 10);
        assert_eq!(
            validate_manual_trigger(&base).unwrap().coordinator,
            coordinator.public_key()
        );

        let event = with_tags(&maintainer, &base, |tags| {
            tags.push(tag(&["p", &Keys::generate().public_key().to_hex()]));
        });
        assert_eq!(
            validate_manual_trigger(&event),
            Err(ShapeReason::RepeatedTag("p"))
        );
    }

    #[test]
    fn pr_context_manual_trigger_rejects_a_second_p() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = manual_trigger(&maintainer, &coordinator, &owner, 10);
        // The NIP-22 context a PR-context trigger carries excludes the
        // participant `p`, so its sole `p` remains the coordinator.
        let pr_context = with_tags(&maintainer, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("r"));
            tags.push(tag(&["E", &Keys::generate().public_key().to_hex()]));
        });
        let parsed = validate_manual_trigger(&pr_context).unwrap();
        assert_eq!(parsed.coordinator, coordinator.public_key());
        assert!(parsed.common.pr_root().is_some());

        let event = with_tags(&maintainer, &pr_context, |tags| {
            tags.push(tag(&["p", &Keys::generate().public_key().to_hex()]));
        });
        assert_eq!(
            validate_manual_trigger(&event),
            Err(ShapeReason::RepeatedTag("p"))
        );
    }

    #[test]
    fn validate_rejects_unconsumed_kinds() {
        let keys = Keys::generate();
        let event = event(&keys, Kind::TextNote, 10, vec![]);
        assert_eq!(
            validate(&event),
            Err(ShapeReason::UnexpectedKind(Kind::TextNote))
        );
    }

    #[test]
    fn common_tags_reject_mismatched_repository_identifiers() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let event = with_tags(&coordinator, &base, |tags| {
            tags.push(tag(&[
                "a",
                &format!(
                    "{}:{}:other",
                    Kind::GitRepoAnnouncement.as_u16(),
                    Keys::generate().public_key().to_hex()
                ),
            ]));
        });
        assert_eq!(
            validate_workflow_result(&event),
            Err(ShapeReason::MismatchedRepositories)
        );
    }
}
