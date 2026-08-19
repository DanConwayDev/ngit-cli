//! Frozen request-provenance validation.
//!
//! A run's `q` quote of a Service Request or Manual Trigger is authored by the
//! coordinator, so on its own it is a claim, not evidence: it names an event
//! id and a requester pubkey the coordinator chose. It becomes evidence only
//! once the quoted event has been retrieved and every part of the claim
//! checked against it — id, kind, signature, shape, author, repository,
//! coordinator and, for a Manual Trigger, the run context it authorized.
//!
//! Nothing here fetches. The caller supplies whatever quoted events it has
//! (from the local nostr cache, or from a relay fetch in the full tier) and
//! receives back one [`ValidatedProvenance`] verdict per run that passed,
//! which is exactly the `validated_provenance` set
//! [`crate::ci::trust::run_trust_resolution`] and
//! [`crate::ci::controls::run_maintainer_link`] take. A verdict names the run
//! it was reached for, because validation is a statement about that run: the
//! same Manual Trigger authorizes one run's workflow and commit and says
//! nothing about the next, so a verdict must never transfer by quoted id
//! alone.
//!
//! Validation is not the whole of coverage. Nothing here asks *when* the
//! request was signed or whether a Stop had already closed it, because that
//! is the control history's question; the verdict therefore carries the
//! validated request forward for [`crate::ci::controls`] to place in time.

use std::{collections::HashMap, fmt};

use nostr::prelude::{Coordinate, Event, EventId, Kind, PublicKey, Timestamp};

use super::{
    events::WorkflowRun,
    kinds::{
        self, KIND_CI_MANUAL_TRIGGER, KIND_CI_SERVICE_REQUEST, ManualTrigger, ProvenanceKind,
        ProvenanceQuote, ServiceControl, ShapeReason,
    },
};

/// The event kind a provenance marker requires.
#[must_use]
pub fn quoted_kind(kind: ProvenanceKind) -> Kind {
    match kind {
        ProvenanceKind::ServiceRequest => KIND_CI_SERVICE_REQUEST,
        ProvenanceKind::ManualTrigger => KIND_CI_MANUAL_TRIGGER,
    }
}

/// Why a quoted request is not evidence for the run that quotes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceRejection {
    /// The run carries no frozen provenance quote at all.
    NoQuote,
    /// The supplied event is not the one the quote names.
    EventIdMismatch { quoted: EventId, supplied: EventId },
    /// The quoted event's kind does not match the quote's marker.
    KindMismatch { expected: Kind, found: Kind },
    /// The quoted event's id or signature does not verify, so its tags are
    /// not the requester's statement whatever they say.
    InvalidSignature,
    /// The quoted event does not match the shape its kind requires.
    Shape(ShapeReason),
    /// The quote's requester pubkey hint is not the quoted event's author.
    RequesterHintMismatch { hint: PublicKey, author: PublicKey },
    /// The request's author is not a confirmed repository maintainer.
    AuthorNotConfirmedMaintainer(PublicKey),
    /// None of the request's `a` coordinates is in the maintainer closure.
    RepositoryOutsideClosure,
    /// The request does not `p` tag the run's coordinator.
    CoordinatorNotAddressed(PublicKey),
    /// The Manual Trigger authorized a different workflow file or content.
    WorkflowMismatch { expected: String, found: String },
    /// The Manual Trigger authorized a different commit.
    CommitMismatch { expected: String, found: String },
    /// The Manual Trigger's pull-request context is not the run's.
    PullRequestContextMismatch {
        expected: Option<EventId>,
        found: Option<EventId>,
    },
}

impl fmt::Display for ProvenanceRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoQuote => write!(f, "the run carries no request-provenance quote"),
            Self::EventIdMismatch { quoted, supplied } => {
                write!(f, "quote names {quoted} but {supplied} was supplied")
            }
            Self::KindMismatch { expected, found } => write!(
                f,
                "quote marker requires kind {expected} but the event is kind {found}"
            ),
            Self::InvalidSignature => {
                write!(f, "the quoted request's id or signature does not verify")
            }
            Self::Shape(reason) => write!(f, "quoted request is malformed: {reason}"),
            Self::RequesterHintMismatch { hint, author } => write!(
                f,
                "quote names requester {hint} but the request was signed by {author}"
            ),
            Self::AuthorNotConfirmedMaintainer(author) => {
                write!(f, "{author} is not a confirmed repository maintainer")
            }
            Self::RepositoryOutsideClosure => write!(
                f,
                "the request names no repository in the maintainer closure"
            ),
            Self::CoordinatorNotAddressed(coordinator) => {
                write!(f, "the request does not address coordinator {coordinator}")
            }
            Self::WorkflowMismatch { expected, found } => {
                write!(f, "the request authorized workflow {found}, not {expected}")
            }
            Self::CommitMismatch { expected, found } => {
                write!(f, "the request authorized commit {found}, not {expected}")
            }
            Self::PullRequestContextMismatch { expected, found } => write!(
                f,
                "the request's pull-request context is {found:?}, not {expected:?}"
            ),
        }
    }
}

impl std::error::Error for ProvenanceRejection {}

/// A run's quote that failed validation, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedProvenance {
    pub coordinator: PublicKey,
    pub run_id: String,
    pub quote: EventId,
    pub reason: ProvenanceRejection,
}

impl fmt::Display for RejectedProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run {} rejected quote {}: {}",
            self.run_id, self.quote, self.reason
        )
    }
}

/// The request a verdict was reached against, carrying what the temporal
/// rules still have to decide.
///
/// Validation says the quoted event really is a maintainer's request for this
/// repository and coordinator. It says nothing about *when*, and a request
/// never retroactively covers a run that was already under way — so the
/// verdict carries the request forward rather than the bare fact that it held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedRequest {
    /// The quoted Service Request itself, so a run's coverage can be reduced
    /// from the control history exactly as an unquoted request's is: a Stop
    /// that closed it before the run started closes it here too.
    ServiceRequest(Box<ServiceControl>),
    /// A Manual Trigger authorizes one run and no Stop closes it, so the only
    /// temporal question is whether it preceded the run it authorized.
    ManualTrigger {
        event_id: EventId,
        created_at: Timestamp,
    },
}

impl ValidatedRequest {
    /// The quoted request's event id.
    #[must_use]
    pub fn event_id(&self) -> EventId {
        match self {
            Self::ServiceRequest(request) => request.event_id,
            Self::ManualTrigger { event_id, .. } => *event_id,
        }
    }
}

/// A run whose frozen quote was checked against the quoted event and held.
///
/// The verdict is scoped to the run it was reached for. A quoted id alone
/// would not be: a Manual Trigger authorizes one workflow, commit and
/// pull-request context, so a run that matches it legitimizes only itself,
/// and a Service Request is checked against the coordinator each run names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedProvenance {
    pub coordinator: PublicKey,
    pub run_id: String,
    /// The request the quote names, as validated.
    pub request: ValidatedRequest,
}

impl ValidatedProvenance {
    /// The verdict for the run whose quote checked out.
    ///
    /// Constructing one is how a run gains maintainer direction, so this stays
    /// inside `ci`: outside it, a verdict is only obtainable from
    /// [`validate_run_provenance`].
    pub(super) fn for_run(run: &WorkflowRun, request: ValidatedRequest) -> Self {
        Self {
            coordinator: run.coordinator,
            run_id: run.run_id.clone(),
            request,
        }
    }

    /// The quoted request's event id.
    #[must_use]
    pub fn quote(&self) -> EventId {
        self.request.event_id()
    }

    /// Whether this verdict was reached for `run`'s own frozen quote.
    #[must_use]
    pub fn covers(&self, run: &WorkflowRun) -> bool {
        self.coordinator == run.coordinator
            && self.run_id == run.run_id
            && run
                .provenance()
                .is_some_and(|quote| quote.event_id == self.quote())
    }
}

/// A quoted request a run needs before its provenance can be checked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WantedQuote {
    pub event_id: EventId,
    /// The kind the marker requires, for a narrow fetch filter.
    pub kind: Kind,
    /// The quote's requester pubkey hint, also for the filter.
    pub requester: PublicKey,
    pub relay: Option<String>,
}

impl From<&ProvenanceQuote> for WantedQuote {
    fn from(quote: &ProvenanceQuote) -> Self {
        Self {
            event_id: quote.event_id,
            kind: quoted_kind(quote.kind),
            requester: quote.requester,
            relay: quote.relay.clone(),
        }
    }
}

/// The outcome of validating every run's frozen quote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvenanceOutcome {
    /// Runs whose quote passed, for `validated_provenance`.
    pub validated: Vec<ValidatedProvenance>,
    pub rejected: Vec<RejectedProvenance>,
    /// Quotes whose event the caller did not supply. These are neither
    /// evidence nor a negative finding: the query is simply unresolved, which
    /// is why the resolve tiers report partial coverage for them.
    pub unavailable: Vec<WantedQuote>,
}

/// The quoted requests the runs reference, deduplicated.
#[must_use]
pub fn wanted_quotes(runs: &[WorkflowRun]) -> Vec<WantedQuote> {
    let mut wanted: Vec<WantedQuote> = Vec::new();
    for quote in runs.iter().filter_map(WorkflowRun::provenance) {
        let candidate = WantedQuote::from(quote);
        if !wanted
            .iter()
            .any(|existing| existing.event_id == candidate.event_id)
        {
            wanted.push(candidate);
        }
    }
    wanted
}

/// Validate one run's frozen provenance quote against the quoted event.
///
/// The verdict is [`ValidatedProvenance`] for *this* run: passing here says
/// nothing about another run that quotes the same request.
///
/// # Errors
///
/// Returns the [`ProvenanceRejection`] the quote failed on. A rejection is
/// never a negative claim about the run — it only means this quote supplies
/// no maintainer-direction evidence.
pub fn validate_run_provenance(
    run: &WorkflowRun,
    quoted: &Event,
    confirmed_maintainers: &[PublicKey],
    closure_coordinates: &[Coordinate],
) -> Result<ValidatedProvenance, ProvenanceRejection> {
    let quote = run.provenance().ok_or(ProvenanceRejection::NoQuote)?;

    if quoted.id != quote.event_id {
        return Err(ProvenanceRejection::EventIdMismatch {
            quoted: quote.event_id,
            supplied: quoted.id,
        });
    }
    let expected_kind = quoted_kind(quote.kind);
    if quoted.kind != expected_kind {
        return Err(ProvenanceRejection::KindMismatch {
            expected: expected_kind,
            found: quoted.kind,
        });
    }
    // Everything below reads the quoted event's tags as the requester's
    // statement, so the event must actually be theirs. Without this, the
    // coordinator could supply a doctored copy of a real maintainer request —
    // same id, `p` tag rewritten to address itself — and pass every remaining
    // check. Verification happens here rather than at the fetch so it holds
    // whatever the caller's source is.
    if quoted.verify().is_err() {
        return Err(ProvenanceRejection::InvalidSignature);
    }
    // The hint is what the fetch was keyed on, so a hint naming someone other
    // than the signer means the coordinator's claim and the event disagree.
    if quote.requester != quoted.pubkey {
        return Err(ProvenanceRejection::RequesterHintMismatch {
            hint: quote.requester,
            author: quoted.pubkey,
        });
    }
    if !confirmed_maintainers.contains(&quoted.pubkey) {
        return Err(ProvenanceRejection::AuthorNotConfirmedMaintainer(
            quoted.pubkey,
        ));
    }

    let request = match quote.kind {
        ProvenanceKind::ServiceRequest => {
            let request =
                kinds::validate_service_control(quoted).map_err(ProvenanceRejection::Shape)?;
            validate_service_request(run, &request, closure_coordinates)?;
            ValidatedRequest::ServiceRequest(Box::new(request))
        }
        ProvenanceKind::ManualTrigger => {
            let trigger =
                kinds::validate_manual_trigger(quoted).map_err(ProvenanceRejection::Shape)?;
            validate_manual_trigger(run, &trigger, closure_coordinates)?;
            ValidatedRequest::ManualTrigger {
                event_id: trigger.event_id,
                created_at: trigger.created_at,
            }
        }
    };

    Ok(ValidatedProvenance::for_run(run, request))
}

/// Validate every run's quote against the supplied events.
///
/// Runs with no quote are not reported: the control-history reduction remains
/// available to them as independent evidence.
#[must_use]
pub fn validate_run_quotes(
    runs: &[WorkflowRun],
    quoted_events: &HashMap<EventId, Event>,
    confirmed_maintainers: &[PublicKey],
    closure_coordinates: &[Coordinate],
) -> ProvenanceOutcome {
    let mut outcome = ProvenanceOutcome::default();
    for run in runs {
        let Some(quote) = run.provenance() else {
            continue;
        };
        let Some(quoted) = quoted_events.get(&quote.event_id) else {
            let wanted = WantedQuote::from(quote);
            if !outcome
                .unavailable
                .iter()
                .any(|existing| existing.event_id == wanted.event_id)
            {
                outcome.unavailable.push(wanted);
            }
            continue;
        };
        match validate_run_provenance(run, quoted, confirmed_maintainers, closure_coordinates) {
            Ok(validated) => {
                if !outcome.validated.contains(&validated) {
                    outcome.validated.push(validated);
                }
            }
            Err(reason) => outcome.rejected.push(RejectedProvenance {
                coordinator: run.coordinator,
                run_id: run.run_id.clone(),
                quote: quote.event_id,
                reason,
            }),
        }
    }
    outcome
}

fn validate_service_request(
    run: &WorkflowRun,
    request: &ServiceControl,
    closure_coordinates: &[Coordinate],
) -> Result<(), ProvenanceRejection> {
    if !request.is_request {
        return Err(ProvenanceRejection::KindMismatch {
            expected: KIND_CI_SERVICE_REQUEST,
            found: kinds::KIND_CI_SERVICE_STOP,
        });
    }
    if !closure_coordinates.contains(&request.repository.coordinate) {
        return Err(ProvenanceRejection::RepositoryOutsideClosure);
    }
    if request.coordinator != run.coordinator {
        return Err(ProvenanceRejection::CoordinatorNotAddressed(
            run.coordinator,
        ));
    }
    // A standing Service Request authorizes the coordinator for the
    // repository, not one run, so there is no run context to match here. When
    // it covered a *particular* run is the control-history reduction's
    // question, which is why the verdict carries the request itself.
    Ok(())
}

fn validate_manual_trigger(
    run: &WorkflowRun,
    trigger: &ManualTrigger,
    closure_coordinates: &[Coordinate],
) -> Result<(), ProvenanceRejection> {
    if !trigger
        .common
        .repositories
        .iter()
        .any(|repository| closure_coordinates.contains(&repository.coordinate))
    {
        return Err(ProvenanceRejection::RepositoryOutsideClosure);
    }
    // Shape validation caps every trigger at one `p`, so the addressee is a
    // single coordinator and a second cannot ride along.
    if trigger.coordinator != run.coordinator {
        return Err(ProvenanceRejection::CoordinatorNotAddressed(
            run.coordinator,
        ));
    }
    if trigger.common.workflow.path != run.workflow_path
        || trigger.common.workflow.sha256 != run.workflow_hash
    {
        return Err(ProvenanceRejection::WorkflowMismatch {
            expected: format!("{}@{}", run.workflow_path, run.workflow_hash),
            found: format!(
                "{}@{}",
                trigger.common.workflow.path, trigger.common.workflow.sha256
            ),
        });
    }
    if trigger.common.commit() != run.commit() {
        return Err(ProvenanceRejection::CommitMismatch {
            expected: run.commit().to_owned(),
            found: trigger.common.commit().to_owned(),
        });
    }
    if trigger.common.pr_root() != run.pr_root {
        return Err(ProvenanceRejection::PullRequestContextMismatch {
            expected: run.pr_root,
            found: trigger.common.pr_root(),
        });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use nostr::prelude::{Keys, Tag};

    use super::{
        super::{
            controls::{RunMaintainerLink, run_maintainer_link, tests::quoted_run},
            kinds::{MARKER_MANUAL_TRIGGER, MARKER_SERVICE_REQUEST, test_events::*},
        },
        *,
    };

    pub(crate) fn perspective(owner: &Keys) -> Coordinate {
        Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: owner.public_key(),
            identifier: "ngit".to_owned(),
        }
    }

    /// A Service Request from `maintainer` and the run that quotes it.
    pub(crate) fn service_request_run(
        coordinator: &Keys,
        owner: &Keys,
        maintainer: &Keys,
    ) -> (Event, WorkflowRun) {
        let request = service_control(maintainer, coordinator, owner, true, 50);
        let run = quoted_run(
            coordinator,
            owner,
            maintainer,
            "run-1",
            request.id,
            MARKER_SERVICE_REQUEST,
        );
        (request, run)
    }

    /// A Manual Trigger from `maintainer` and the run that quotes it.
    pub(crate) fn manual_trigger_run(
        coordinator: &Keys,
        owner: &Keys,
        maintainer: &Keys,
    ) -> (Event, WorkflowRun) {
        let trigger = manual_trigger(maintainer, coordinator, owner, 50);
        let run = quoted_run(
            coordinator,
            owner,
            maintainer,
            "run-1",
            trigger.id,
            MARKER_MANUAL_TRIGGER,
        );
        (trigger, run)
    }

    /// Rebuild `trigger` with `f` applied to its tags, and the run quoting it.
    fn manual_trigger_run_with(
        coordinator: &Keys,
        owner: &Keys,
        maintainer: &Keys,
        f: impl FnOnce(&mut Vec<Tag>),
    ) -> (Event, WorkflowRun) {
        let base = manual_trigger(maintainer, coordinator, owner, 50);
        let trigger = with_tags(maintainer, &base, f);
        let run = quoted_run(
            coordinator,
            owner,
            maintainer,
            "run-1",
            trigger.id,
            MARKER_MANUAL_TRIGGER,
        );
        (trigger, run)
    }

    #[test]
    fn a_maintainers_service_request_validates() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);

        let validated = validate_run_provenance(
            &run,
            &request,
            &[maintainer.public_key()],
            &[perspective(&owner)],
        )
        .expect("a maintainer's request for this coordinator validates");
        assert!(validated.covers(&run));
        assert_eq!(validated.quote(), request.id);
        // The verdict carries the request itself: whether it was standing
        // when this run started is the control history's question.
        let ValidatedRequest::ServiceRequest(carried) = &validated.request else {
            panic!("a service-request quote validates to the request");
        };
        assert_eq!(carried.author, maintainer.public_key());
        assert_eq!(carried.created_at, request.created_at);
    }

    #[test]
    fn a_maintainers_manual_trigger_validates_with_its_run_context() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (trigger, run) = manual_trigger_run(&coordinator, &owner, &maintainer);

        let validated = validate_run_provenance(
            &run,
            &trigger,
            &[maintainer.public_key()],
            &[perspective(&owner)],
        )
        .expect("a maintainer's trigger for this run's context validates");
        assert!(validated.covers(&run));
        assert_eq!(
            validated.request,
            ValidatedRequest::ManualTrigger {
                event_id: trigger.id,
                created_at: trigger.created_at,
            },
            "the verdict carries when the trigger was signed",
        );
    }

    #[test]
    fn a_run_without_a_quote_has_no_provenance() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let request = service_control(&maintainer, &coordinator, &owner, true, 50);
        let grouped = super::super::events::group_workflow_runs(&[workflow_result(
            &coordinator,
            &owner,
            "run-1",
            100,
        )]);
        let run = grouped.runs.into_iter().next().unwrap();

        assert_eq!(
            validate_run_provenance(
                &run,
                &request,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::NoQuote)
        );
    }

    #[test]
    fn another_event_cannot_stand_in_for_the_quoted_one() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (_, run) = service_request_run(&coordinator, &owner, &maintainer);
        // A different, equally valid Request from the same maintainer.
        let other = service_control(&maintainer, &coordinator, &owner, true, 60);

        assert!(matches!(
            validate_run_provenance(
                &run,
                &other,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::EventIdMismatch { .. })
        ));
    }

    #[test]
    fn the_marker_must_match_the_quoted_kind() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // The coordinator marks a Manual Trigger as a Service Request.
        let trigger = manual_trigger(&maintainer, &coordinator, &owner, 50);
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            trigger.id,
            MARKER_SERVICE_REQUEST,
        );

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::KindMismatch {
                expected: KIND_CI_SERVICE_REQUEST,
                found: KIND_CI_MANUAL_TRIGGER,
            })
        );
    }

    #[test]
    fn a_service_stop_is_not_a_service_request() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let stop = service_control(&maintainer, &coordinator, &owner, false, 50);
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            stop.id,
            MARKER_SERVICE_REQUEST,
        );

        assert!(matches!(
            validate_run_provenance(
                &run,
                &stop,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::KindMismatch { .. })
        ));
    }

    #[test]
    fn a_malformed_request_is_rejected_with_its_shape_reason() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let base = service_control(&maintainer, &coordinator, &owner, true, 50);
        let malformed = with_tags(&maintainer, &base, |tags| {
            tags.push(tag(&["p", &Keys::generate().public_key().to_hex()]));
        });
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            malformed.id,
            MARKER_SERVICE_REQUEST,
        );

        assert_eq!(
            validate_run_provenance(
                &run,
                &malformed,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::Shape(ShapeReason::RepeatedTag("p")))
        );
    }

    #[test]
    fn a_doctored_copy_of_a_real_request_is_rejected() {
        let coordinator = Keys::generate();
        let addressed = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // A real request, addressed to another coordinator.
        let request = service_control(&maintainer, &addressed, &owner, true, 50);
        // The coordinator quotes its real id but supplies a copy whose `p`
        // tag addresses itself. Every other check would pass.
        let doctored = Event::new(
            request.id,
            request.pubkey,
            request.created_at,
            request.kind,
            vec![
                tag(&["a", &repo_coordinate(&owner)]),
                tag(&["p", &coordinator.public_key().to_hex()]),
            ],
            request.content.clone(),
            request.sig,
        );
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            request.id,
            MARKER_SERVICE_REQUEST,
        );

        assert_eq!(
            validate_run_provenance(
                &run,
                &doctored,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::InvalidSignature)
        );
        // The genuine event still validates only for the coordinator it named.
        assert_eq!(
            validate_run_provenance(
                &run,
                &request,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::CoordinatorNotAddressed(
                coordinator.public_key()
            ))
        );
    }

    #[test]
    fn a_request_from_a_non_maintainer_is_not_maintainer_direction() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let stranger = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &stranger);

        assert_eq!(
            validate_run_provenance(
                &run,
                &request,
                &[Keys::generate().public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::AuthorNotConfirmedMaintainer(
                stranger.public_key()
            ))
        );
    }

    #[test]
    fn a_request_for_another_repository_does_not_transfer() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let other_owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);

        assert_eq!(
            validate_run_provenance(
                &run,
                &request,
                &[maintainer.public_key()],
                &[perspective(&other_owner)]
            ),
            Err(ProvenanceRejection::RepositoryOutsideClosure)
        );
    }

    #[test]
    fn a_manual_trigger_for_another_repository_does_not_transfer() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let other_owner = Keys::generate();
        let maintainer = Keys::generate();
        let (trigger, run) = manual_trigger_run(&coordinator, &owner, &maintainer);

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&other_owner)]
            ),
            Err(ProvenanceRejection::RepositoryOutsideClosure)
        );
    }

    #[test]
    fn the_requester_hint_must_name_the_signer() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let other_maintainer = Keys::generate();
        let request = service_control(&maintainer, &coordinator, &owner, true, 50);
        // The coordinator credits a maintainer who signed nothing.
        let run = quoted_run(
            &coordinator,
            &owner,
            &other_maintainer,
            "run-1",
            request.id,
            MARKER_SERVICE_REQUEST,
        );

        assert_eq!(
            validate_run_provenance(
                &run,
                &request,
                &[maintainer.public_key(), other_maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::RequesterHintMismatch {
                hint: other_maintainer.public_key(),
                author: maintainer.public_key(),
            })
        );
    }

    #[test]
    fn a_request_addressed_to_another_coordinator_does_not_cover_this_run() {
        let coordinator = Keys::generate();
        let addressed = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let request = service_control(&maintainer, &addressed, &owner, true, 50);
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            request.id,
            MARKER_SERVICE_REQUEST,
        );

        assert_eq!(
            validate_run_provenance(
                &run,
                &request,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::CoordinatorNotAddressed(
                coordinator.public_key()
            ))
        );
    }

    #[test]
    fn a_manual_trigger_addressed_to_another_coordinator_does_not_cover_this_run() {
        let coordinator = Keys::generate();
        let addressed = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let trigger = manual_trigger(&maintainer, &addressed, &owner, 50);
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            trigger.id,
            MARKER_MANUAL_TRIGGER,
        );

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::CoordinatorNotAddressed(
                coordinator.public_key()
            ))
        );
    }

    #[test]
    fn a_manual_trigger_for_another_workflow_does_not_cover_this_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (trigger, run) = manual_trigger_run_with(&coordinator, &owner, &maintainer, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("w"));
            tags.push(tag(&[
                "w",
                ".ngit/act/workflows/release.yml",
                WORKFLOW_HASH,
            ]));
        });

        assert!(matches!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::WorkflowMismatch { .. })
        ));
    }

    #[test]
    fn a_manual_trigger_for_another_workflow_revision_does_not_cover_this_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (trigger, run) = manual_trigger_run_with(&coordinator, &owner, &maintainer, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("w"));
            tags.push(tag(&["w", WORKFLOW_PATH, &"3".repeat(64)]));
        });

        assert!(matches!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::WorkflowMismatch { .. })
        ));
    }

    #[test]
    fn a_manual_trigger_for_another_commit_does_not_cover_this_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (trigger, run) = manual_trigger_run_with(&coordinator, &owner, &maintainer, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("c"));
            tags.push(tag(&["c", &"9".repeat(40)]));
        });

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::CommitMismatch {
                expected: COMMIT.to_owned(),
                found: "9".repeat(40),
            })
        );
    }

    #[test]
    fn a_manual_trigger_for_another_pull_request_does_not_cover_this_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let pr_root = EventId::from_slice(&[0x5a; 32]).unwrap();
        // The run is a push run; the trigger authorized a PR run instead.
        let (trigger, run) = manual_trigger_run_with(&coordinator, &owner, &maintainer, |tags| {
            tags.retain(|tag| tag.as_slice().get(1).map(String::as_str) != Some("refs/heads/main"));
            tags.push(tag(&["E", &pr_root.to_hex()]));
        });

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::PullRequestContextMismatch {
                expected: None,
                found: Some(pr_root),
            })
        );
    }

    /// A pull-request run quoting `quote_id`.
    fn pull_request_run(
        coordinator: &Keys,
        owner: &Keys,
        requester: &Keys,
        pr_root: EventId,
        quote_id: EventId,
    ) -> WorkflowRun {
        let base = workflow_result(coordinator, owner, "run-1", 100);
        let event = with_tags(coordinator, &base, |tags| {
            // Replace the push context with the NIP-22 pull-request context,
            // keeping the workflow-run `r`.
            tags.retain(|tag| {
                let slice = tag.as_slice();
                slice.first().map(String::as_str) != Some("o")
                    && slice.get(1).map(String::as_str) != Some("refs/heads/main")
            });
            tags.extend([
                tag(&["o", "pull_request"]),
                tag(&["E", &pr_root.to_hex()]),
                tag(&[
                    "q",
                    &quote_id.to_hex(),
                    "wss://relay.example",
                    &requester.public_key().to_hex(),
                    MARKER_MANUAL_TRIGGER,
                ]),
            ]);
        });
        let grouped = super::super::events::group_workflow_runs(std::slice::from_ref(&event));
        assert!(grouped.skipped.is_empty(), "{:?}", grouped.skipped);
        grouped.runs.into_iter().next().expect("one run")
    }

    #[test]
    fn a_branch_manual_trigger_does_not_cover_a_pull_request_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let pr_root = EventId::from_slice(&[0x5a; 32]).unwrap();
        // The maintainer authorized the commit on a branch; the coordinator
        // replayed it as a pull-request run.
        let trigger = manual_trigger(&maintainer, &coordinator, &owner, 50);
        let run = pull_request_run(&coordinator, &owner, &maintainer, pr_root, trigger.id);

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::PullRequestContextMismatch {
                expected: Some(pr_root),
                found: None,
            })
        );
    }

    #[test]
    fn a_manual_trigger_for_one_pull_request_does_not_cover_another() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let authorized = EventId::from_slice(&[0x5a; 32]).unwrap();
        let other = EventId::from_slice(&[0x5b; 32]).unwrap();
        let base = manual_trigger(&maintainer, &coordinator, &owner, 50);
        let trigger = with_tags(&maintainer, &base, |tags| {
            tags.retain(|tag| tag.as_slice().get(1).map(String::as_str) != Some("refs/heads/main"));
            tags.push(tag(&["E", &authorized.to_hex()]));
        });
        let run = pull_request_run(&coordinator, &owner, &maintainer, other, trigger.id);

        assert_eq!(
            validate_run_provenance(
                &run,
                &trigger,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::PullRequestContextMismatch {
                expected: Some(other),
                found: Some(authorized),
            })
        );
    }

    #[test]
    fn a_pull_request_manual_trigger_addresses_only_the_coordinator() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let pr_root = EventId::from_slice(&[0x5a; 32]).unwrap();
        let base = manual_trigger(&maintainer, &coordinator, &owner, 50);
        // The NIP-22 context a PR-context trigger carries excludes the
        // participant `p`, so the coordinator remains its sole addressee.
        let trigger = with_tags(&maintainer, &base, |tags| {
            tags.retain(|tag| tag.as_slice().get(1).map(String::as_str) != Some("refs/heads/main"));
            tags.push(tag(&["E", &pr_root.to_hex()]));
        });
        let run = pull_request_run(&coordinator, &owner, &maintainer, pr_root, trigger.id);
        let validated = validate_run_provenance(
            &run,
            &trigger,
            &[maintainer.public_key()],
            &[perspective(&owner)],
        )
        .expect("a coordinator-addressed pull-request trigger validates");
        assert!(validated.covers(&run));

        // One that names a further party — the PR author a Workflow Result
        // tags — is malformed, which is what the coordinator concludes too.
        let with_participant = with_tags(&maintainer, &trigger, |tags| {
            tags.push(tag(&["p", &owner.public_key().to_hex()]));
        });
        let run = pull_request_run(
            &coordinator,
            &owner,
            &maintainer,
            pr_root,
            with_participant.id,
        );
        assert_eq!(
            validate_run_provenance(
                &run,
                &with_participant,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            Err(ProvenanceRejection::Shape(ShapeReason::RepeatedTag("p"))),
        );
    }

    #[test]
    fn unavailable_quotes_are_reported_rather_than_rejected() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);

        let outcome = validate_run_quotes(
            std::slice::from_ref(&run),
            &HashMap::new(),
            &[maintainer.public_key()],
            &[perspective(&owner)],
        );
        assert!(outcome.validated.is_empty());
        assert!(outcome.rejected.is_empty());
        assert_eq!(
            outcome.unavailable,
            vec![WantedQuote {
                event_id: request.id,
                kind: KIND_CI_SERVICE_REQUEST,
                requester: maintainer.public_key(),
                relay: Some("wss://relay.example".to_owned()),
            }]
        );
        assert_eq!(
            outcome.unavailable,
            wanted_quotes(std::slice::from_ref(&run))
        );
    }

    #[test]
    fn supplied_quotes_are_validated_per_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let stranger = Keys::generate();
        let (request, requested_run) = service_request_run(&coordinator, &owner, &maintainer);
        let strangers_request = service_control(&stranger, &coordinator, &owner, true, 50);
        let strangers_run = quoted_run(
            &coordinator,
            &owner,
            &stranger,
            "run-2",
            strangers_request.id,
            MARKER_SERVICE_REQUEST,
        );

        let quoted: HashMap<EventId, Event> = [
            (request.id, request.clone()),
            (strangers_request.id, strangers_request.clone()),
        ]
        .into_iter()
        .collect();
        let runs = [requested_run, strangers_run];
        let outcome = validate_run_quotes(
            &runs,
            &quoted,
            &[maintainer.public_key()],
            &[perspective(&owner)],
        );
        assert_eq!(outcome.validated.len(), 1);
        assert!(outcome.validated[0].covers(&runs[0]));
        assert_eq!(outcome.validated[0].quote(), request.id);
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].quote, strangers_request.id);
        assert!(outcome.unavailable.is_empty());
    }

    /// A run that quotes `quote_id` but ran a different commit.
    pub(crate) fn run_on_another_commit(
        coordinator: &Keys,
        owner: &Keys,
        requester: &Keys,
        run_id: &str,
        quote_id: EventId,
    ) -> WorkflowRun {
        let base = workflow_result(coordinator, owner, run_id, 100);
        let event = with_tags(coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("c"));
            tags.extend([
                tag(&["c", &"9".repeat(40)]),
                tag(&[
                    "q",
                    &quote_id.to_hex(),
                    "wss://relay.example",
                    &requester.public_key().to_hex(),
                    MARKER_MANUAL_TRIGGER,
                ]),
            ]);
        });
        let grouped = super::super::events::group_workflow_runs(std::slice::from_ref(&event));
        assert!(grouped.skipped.is_empty(), "{:?}", grouped.skipped);
        grouped.runs.into_iter().next().expect("one run")
    }

    #[test]
    fn a_validated_trigger_does_not_legitimize_another_run_quoting_it() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // Both runs quote the same Manual Trigger; only `authorized` ran the
        // workflow and commit the maintainer authorized.
        let (trigger, authorized) = manual_trigger_run(&coordinator, &owner, &maintainer);
        let replayed =
            run_on_another_commit(&coordinator, &owner, &maintainer, "run-2", trigger.id);
        let quoted: HashMap<EventId, Event> = [(trigger.id, trigger.clone())].into_iter().collect();
        let maintainers = [maintainer.public_key()];

        let runs = [authorized, replayed];
        let outcome = validate_run_quotes(&runs, &quoted, &maintainers, &[perspective(&owner)]);
        assert_eq!(outcome.validated.len(), 1);
        assert!(
            outcome.validated[0].covers(&runs[0]),
            "the verdict names the run it was reached for"
        );
        assert_eq!(outcome.validated[0].quote(), trigger.id);
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].run_id, "run-2");

        // The verdict covers only that run, so the mismatching one gains no
        // maintainer direction from its neighbour's quote.
        assert_eq!(
            run_maintainer_link(&runs[0], &maintainers, &[], &outcome.validated),
            Some(RunMaintainerLink::Manual)
        );
        assert_eq!(
            run_maintainer_link(&runs[1], &maintainers, &[], &outcome.validated),
            None
        );
    }
}
