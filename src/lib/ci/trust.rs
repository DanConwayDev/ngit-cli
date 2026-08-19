//! CI trust context: evidence, classification, resolution and rollups.
//!
//! A port of gitworkshop's `src/lib/ciTrustContext.ts` and the pure
//! relationship-evidence mapping from `src/hooks/useCITrustContext.ts`. The
//! canonical labels and evidence wording are reproduced verbatim so the CLI
//! and the web client describe the same evidence in the same words.
//!
//! Trust context describes why a result may deserve attention. Absence of
//! evidence is [`TrustClassification::NoKnownContext`] — never "untrusted".

use std::collections::HashMap;

use nostr::prelude::{Coordinate, PublicKey};

use super::{
    controls::{
        CoordinatorRelationship, CoordinatorRelationshipLevel, RunMaintainerLink,
        run_maintainer_link, run_requester, was_service_requested_when_run_started,
    },
    events::WorkflowRun,
    kinds::{JobResult, ServiceControl},
    provenance::ValidatedProvenance,
};

/// The canonical caveat shown when evidence queries did not all settle.
pub const CONTEXT_INCOMPLETE_LABEL: &str = "Context incomplete";

/// How directly evidence connects a signer to the repository or the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustClassification {
    MaintainerDirected,
    OperationallyAssociated,
    SociallyCorroborated,
    /// Absence of evidence, not a statement that the signer is unsafe.
    NoKnownContext,
}

impl TrustClassification {
    /// Lower ranks are stronger.
    fn rank(self) -> u8 {
        match self {
            Self::MaintainerDirected => 0,
            Self::OperationallyAssociated => 1,
            Self::SociallyCorroborated => 2,
            Self::NoKnownContext => 3,
        }
    }

    /// The machine-readable value shared with gitworkshop and the JSON
    /// output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaintainerDirected => "maintainer-directed",
            Self::OperationallyAssociated => "operationally-associated",
            Self::SociallyCorroborated => "socially-corroborated",
            Self::NoKnownContext => "no-known-context",
        }
    }

    /// Canonical human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::MaintainerDirected => "Maintainer-directed",
            Self::OperationallyAssociated => "Operationally associated",
            Self::SociallyCorroborated => "Socially corroborated",
            Self::NoKnownContext => "No known context",
        }
    }

    /// Canonical human description.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::MaintainerDirected => {
                "A confirmed repository maintainer requested this coordinator service or this particular run."
            }
            Self::OperationallyAssociated => {
                "Signed or independently verified evidence connects this identity to repository-listed infrastructure or a recognized coordinator."
            }
            Self::SociallyCorroborated => {
                "A confirmed maintainer you follow currently requests this CI identity, or it has CI activity on a repository they maintain."
            }
            Self::NoKnownContext => {
                "No maintainer, repository-infrastructure, coordinator, or viewer-relative social evidence was found."
            }
        }
    }
}

/// The classification an evidence item can carry.
///
/// `NoKnownContext` is deliberately not representable: it describes the
/// absence of evidence, so no evidence item can assert it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvidenceClassification {
    MaintainerDirected,
    OperationallyAssociated,
    SociallyCorroborated,
}

impl From<EvidenceClassification> for TrustClassification {
    fn from(value: EvidenceClassification) -> Self {
        match value {
            EvidenceClassification::MaintainerDirected => Self::MaintainerDirected,
            EvidenceClassification::OperationallyAssociated => Self::OperationallyAssociated,
            EvidenceClassification::SociallyCorroborated => Self::SociallyCorroborated,
        }
    }
}

/// The kind of evidence an item records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustEvidenceKind {
    MaintainerRequest,
    HistoricalMaintainerRequest,
    RepositoryDomain,
    RepositorySubdomain,
    CoordinatorDelegation,
    ContactRequest,
    SocialActivity,
}

impl TrustEvidenceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaintainerRequest => "maintainer-request",
            Self::HistoricalMaintainerRequest => "historical-maintainer-request",
            Self::RepositoryDomain => "repository-domain",
            Self::RepositorySubdomain => "repository-subdomain",
            Self::CoordinatorDelegation => "coordinator-delegation",
            Self::ContactRequest => "contact-request",
            Self::SocialActivity => "social-activity",
        }
    }
}

/// Whether an item describes the current relationship, an earlier one, or one
/// run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvidenceScope {
    Current,
    Historical,
    Run,
}

impl EvidenceScope {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Historical => "historical",
            Self::Run => "run",
        }
    }
}

/// One typed piece of trust evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustEvidence {
    pub kind: TrustEvidenceKind,
    pub classification: EvidenceClassification,
    pub summary: String,
    pub detail: String,
    /// Pubkeys that signed the request or activity supporting this evidence.
    pub authors: Vec<PublicKey>,
    pub scope: EvidenceScope,
}

/// Whether every relevant evidence query settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Coverage {
    Complete,
    Partial,
}

impl Coverage {
    /// Combine two coverages: any partial input makes the result partial.
    #[must_use]
    pub fn combine(self, other: Self) -> Self {
        if self == Self::Partial || other == Self::Partial {
            Self::Partial
        } else {
            Self::Complete
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

/// Resolution state, which is independent from the classification: a signer
/// must never be shown as "No known context" while queries are unsettled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustResolution {
    Loading,
    Settled {
        classification: TrustClassification,
        evidence: Vec<TrustEvidence>,
        coverage: Coverage,
    },
}

impl TrustResolution {
    #[must_use]
    pub fn classification(&self) -> Option<TrustClassification> {
        match self {
            Self::Loading => None,
            Self::Settled { classification, .. } => Some(*classification),
        }
    }

    #[must_use]
    pub fn coverage(&self) -> Option<Coverage> {
        match self {
            Self::Loading => None,
            Self::Settled { coverage, .. } => Some(*coverage),
        }
    }

    #[must_use]
    pub fn evidence(&self) -> &[TrustEvidence] {
        match self {
            Self::Loading => &[],
            Self::Settled { evidence, .. } => evidence,
        }
    }
}

/// Per-signer resolutions for one repository view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustContextState {
    Loading,
    Settled {
        resolutions: HashMap<PublicKey, TrustResolution>,
        coverage: Coverage,
    },
}

/// The strongest evidence item's classification; no evidence is
/// [`TrustClassification::NoKnownContext`].
#[must_use]
pub fn classify_trust_evidence(evidence: &[TrustEvidence]) -> TrustClassification {
    evidence
        .iter()
        .map(|item| TrustClassification::from(item.classification))
        .min_by_key(|classification| classification.rank())
        .unwrap_or(TrustClassification::NoKnownContext)
}

/// Build a settled resolution, classifying its evidence.
#[must_use]
pub fn settled_trust_resolution(
    evidence: Vec<TrustEvidence>,
    coverage: Coverage,
) -> TrustResolution {
    TrustResolution::Settled {
        classification: classify_trust_evidence(&evidence),
        evidence,
        coverage,
    }
}

/// The identity-level resolution for one signer.
#[must_use]
pub fn trust_resolution(state: Option<&TrustContextState>, pubkey: &PublicKey) -> TrustResolution {
    match state {
        None | Some(TrustContextState::Loading) => TrustResolution::Loading,
        Some(TrustContextState::Settled {
            resolutions,
            coverage,
        }) => resolutions
            .get(pubkey)
            .cloned()
            .unwrap_or_else(|| settled_trust_resolution(Vec::new(), *coverage)),
    }
}

/// Resolution for one run: apply immutable run provenance without treating
/// today's standing request as retroactive.
///
/// Identity-level `maintainer-request` evidence with `current` scope is
/// dropped, then run-scoped evidence is added when either
///
/// - a verdict in `validated_provenance` — the quoted requests already fetched
///   and checked, each scoped to the run it was checked against — covers this
///   run, names a confirmed maintainer, and the request it names was standing
///   when the run started; or
/// - reducing the immutable control history shows a maintainer's Service
///   Request was active when the run started.
///
/// Both routes therefore answer the same temporal question, because a quote is
/// the coordinator's choice of which request to cite and not a statement about
/// when it applied.
///
/// An unvalidated quote contributes nothing: it is a coordinator-authored tag
/// naming a pubkey, not evidence that the maintainer asked for anything.
#[must_use]
pub fn run_trust_resolution(
    state: Option<&TrustContextState>,
    run: &WorkflowRun,
    confirmed_maintainers: &[PublicKey],
    service_controls: &[ServiceControl],
    perspectives: &[Coordinate],
    validated_provenance: &[ValidatedProvenance],
) -> TrustResolution {
    let TrustResolution::Settled {
        evidence, coverage, ..
    } = trust_resolution(state, &run.coordinator)
    else {
        return TrustResolution::Loading;
    };

    let mut evidence: Vec<TrustEvidence> = evidence
        .into_iter()
        .filter(|item| {
            item.kind != TrustEvidenceKind::MaintainerRequest
                || item.scope != EvidenceScope::Current
        })
        .collect();

    let maintainer_link = run_maintainer_link(
        run,
        confirmed_maintainers,
        service_controls,
        validated_provenance,
    );
    let service_requested_at_run = was_service_requested_when_run_started(
        run,
        service_controls,
        confirmed_maintainers,
        perspectives,
    );

    if maintainer_link == Some(RunMaintainerLink::Manual) {
        evidence.insert(
            0,
            TrustEvidence {
                kind: TrustEvidenceKind::MaintainerRequest,
                classification: EvidenceClassification::MaintainerDirected,
                summary: "Requested by a maintainer".to_owned(),
                detail: "A confirmed repository maintainer manually requested this workflow run."
                    .to_owned(),
                authors: run_requester(run).into_iter().collect(),
                scope: EvidenceScope::Run,
            },
        );
    } else if maintainer_link == Some(RunMaintainerLink::Service)
        || service_requested_at_run == Some(true)
    {
        evidence.insert(
            0,
            TrustEvidence {
                kind: TrustEvidenceKind::MaintainerRequest,
                classification: EvidenceClassification::MaintainerDirected,
                summary: "Covered by a maintainer request".to_owned(),
                detail: "A confirmed repository maintainer's service request was active when this workflow run started."
                    .to_owned(),
                // Only a validated quote names the requester. On the
                // control-history path the covering Request is not the run's
                // quote, so no author is attributed here.
                authors: if maintainer_link == Some(RunMaintainerLink::Service) {
                    run_requester(run).into_iter().collect()
                } else {
                    Vec::new()
                },
                scope: EvidenceScope::Run,
            },
        );
    }

    settled_trust_resolution(evidence, coverage)
}

/// Resolution for one Job Result.
///
/// When the provider signed with a different key than the coordinator,
/// coordinator trust reaches the provider only through the Workflow Result
/// that accepts that job, only for that job, and downgraded: never above
/// operationally associated, except that socially corroborated stays socially
/// corroborated. Provider evidence never flows back to the coordinator.
#[must_use]
pub fn job_trust_resolution(
    state: Option<&TrustContextState>,
    run: &WorkflowRun,
    job: &JobResult,
) -> TrustResolution {
    let provider = trust_resolution(state, &job.author);
    let coordinator = trust_resolution(state, &run.coordinator);
    let (
        TrustResolution::Settled {
            evidence: provider_evidence,
            coverage: provider_coverage,
            ..
        },
        TrustResolution::Settled {
            classification: coordinator_classification,
            coverage: coordinator_coverage,
            ..
        },
    ) = (provider, coordinator)
    else {
        return TrustResolution::Loading;
    };

    if !run.result_accepts_job(job)
        || job.author == run.coordinator
        || coordinator_classification == TrustClassification::NoKnownContext
    {
        return settled_trust_resolution(provider_evidence, provider_coverage);
    }

    let socially_corroborated =
        coordinator_classification == TrustClassification::SociallyCorroborated;
    let mut evidence = provider_evidence;
    evidence.push(TrustEvidence {
        kind: TrustEvidenceKind::CoordinatorDelegation,
        classification: if socially_corroborated {
            EvidenceClassification::SociallyCorroborated
        } else {
            EvidenceClassification::OperationallyAssociated
        },
        summary: if socially_corroborated {
            "Accepted by a socially corroborated coordinator".to_owned()
        } else {
            "Accepted by the coordinator".to_owned()
        },
        detail: if socially_corroborated {
            "The coordinator signed a Workflow Result accepting this provider's Job Result, and that coordinator has CI history near your follow graph. This association is scoped to this job.".to_owned()
        } else {
            "The independently contextual coordinator signed a Workflow Result accepting this provider's Job Result. This association is scoped to this job.".to_owned()
        },
        authors: Vec::new(),
        scope: EvidenceScope::Run,
    });

    settled_trust_resolution(evidence, provider_coverage.combine(coordinator_coverage))
}

/// Roll up several runs conservatively: the least-contextual settled run
/// remains visible, and any partial coverage makes the rollup partial.
#[must_use]
pub fn summarize_run_trust(resolutions: &[TrustResolution]) -> TrustResolution {
    if resolutions
        .iter()
        .any(|resolution| matches!(resolution, TrustResolution::Loading))
    {
        return TrustResolution::Loading;
    }

    let mut weakest: Option<(&TrustClassification, &Vec<TrustEvidence>)> = None;
    let mut coverage = Coverage::Complete;
    for resolution in resolutions {
        let TrustResolution::Settled {
            classification,
            evidence,
            coverage: resolution_coverage,
        } = resolution
        else {
            continue;
        };
        coverage = coverage.combine(*resolution_coverage);
        if weakest.is_none_or(|(current, _)| classification.rank() > current.rank()) {
            weakest = Some((classification, evidence));
        }
    }

    match weakest {
        None => settled_trust_resolution(Vec::new(), Coverage::Complete),
        Some((classification, evidence)) => TrustResolution::Settled {
            classification: *classification,
            evidence: evidence.clone(),
            coverage,
        },
    }
}

/// Identity-level evidence contributed by a coordinator relationship tier.
///
/// The remaining evidence sources — verified NIP-05 domains and viewer-
/// relative social corroboration — arrive in later work packages.
#[must_use]
pub fn relationship_evidence(relationship: Option<&CoordinatorRelationship>) -> Vec<TrustEvidence> {
    let Some(relationship) = relationship else {
        return Vec::new();
    };
    match relationship.level {
        CoordinatorRelationshipLevel::Requested => vec![TrustEvidence {
            kind: TrustEvidenceKind::MaintainerRequest,
            classification: EvidenceClassification::MaintainerDirected,
            summary: "Requested by repository maintainers".to_owned(),
            detail: "A confirmed repository maintainer currently asks this coordinator to run CI for this repository."
                .to_owned(),
            authors: relationship.requester_pubkeys.clone(),
            scope: EvidenceScope::Current,
        }],
        CoordinatorRelationshipLevel::PreviouslyRequested => {
            let manual_direction = relationship.manual_run_count > 0;
            let service_direction = relationship.service_run_count > 0;
            let detail = if manual_direction {
                let count = relationship.manual_run_count;
                let runs = if count == 1 { "run" } else { "runs" };
                let suffix = if service_direction {
                    " Earlier runs also cite a maintainer service request."
                } else {
                    " This does not establish a standing service request."
                };
                format!(
                    "A confirmed repository maintainer manually requested {count} {runs} from this coordinator.{suffix}"
                )
            } else if service_direction {
                "Earlier runs cite a maintainer service request, but there is no active standing request now."
                    .to_owned()
            } else {
                "A confirmed repository maintainer previously signed a service request for this coordinator, but there is no active standing request now."
                    .to_owned()
            };
            vec![TrustEvidence {
                kind: TrustEvidenceKind::HistoricalMaintainerRequest,
                classification: EvidenceClassification::OperationallyAssociated,
                summary: "Earlier maintainer direction".to_owned(),
                detail,
                authors: relationship.requester_pubkeys.clone(),
                scope: EvidenceScope::Historical,
            }]
        }
        CoordinatorRelationshipLevel::Unassociated => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{Event, EventId, Keys, Timestamp};

    use super::{
        super::{
            controls::tests::{control, quoted_run, validated, validated_at},
            events::group_workflow_runs,
            kinds::{KIND_REPO_ANNOUNCEMENT, test_events::*},
            provenance::ValidatedRequest,
        },
        *,
    };

    /// The id of the request a run quotes. Tests build a verdict for it with
    /// `validated` only when the quote is meant to have been fetched and
    /// checked for that run.
    fn request_id() -> EventId {
        EventId::from_slice(&[0xaa; 32]).unwrap()
    }

    fn perspective(owner: &Keys) -> Coordinate {
        Coordinate {
            kind: KIND_REPO_ANNOUNCEMENT,
            public_key: owner.public_key(),
            identifier: "ngit".to_owned(),
        }
    }

    fn evidence(
        kind: TrustEvidenceKind,
        classification: EvidenceClassification,
        scope: EvidenceScope,
    ) -> TrustEvidence {
        TrustEvidence {
            kind,
            classification,
            summary: "summary".to_owned(),
            detail: "detail".to_owned(),
            authors: Vec::new(),
            scope,
        }
    }

    fn settled_state(
        entries: Vec<(PublicKey, TrustResolution)>,
        coverage: Coverage,
    ) -> TrustContextState {
        TrustContextState::Settled {
            resolutions: entries.into_iter().collect(),
            coverage,
        }
    }

    fn run_from(events: &[Event]) -> WorkflowRun {
        let grouped = group_workflow_runs(events);
        assert!(grouped.skipped.is_empty(), "{:?}", grouped.skipped);
        grouped.runs.into_iter().next().expect("one run")
    }

    /// A run whose result carries a frozen provenance quote.
    fn run_with_quote(
        coordinator: &Keys,
        owner: &Keys,
        requester: &Keys,
        marker: &str,
    ) -> WorkflowRun {
        quoted_run(coordinator, owner, requester, "run-1", request_id(), marker)
    }

    #[test]
    fn an_unvalidated_quote_alone_is_not_maintainer_direction() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "manual-trigger");
        let state = settled_state(Vec::new(), Coverage::Complete);

        // The quote names a confirmed maintainer, but it is the
        // coordinator's own tag and nothing has fetched the quoted request.
        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert!(resolution.evidence().is_empty());
    }

    #[test]
    fn a_validated_quote_for_another_run_does_not_transfer() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "manual-trigger");
        // A sibling run quoting the same request, whose quote checked out
        // against *its* context.
        let sibling = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-2",
            request_id(),
            "manual-trigger",
        );
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[
                validated(&sibling),
                ValidatedProvenance {
                    coordinator: coordinator.public_key(),
                    run_id: run.run_id.clone(),
                    request: ValidatedRequest::ManualTrigger {
                        event_id: EventId::from_slice(&[0xcc; 32]).unwrap(),
                        created_at: Timestamp::from_secs(50),
                    },
                },
            ],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext),
            "neither another run's verdict nor another quote's is this run's"
        );
    }

    #[test]
    fn empty_evidence_is_no_known_context() {
        assert_eq!(
            classify_trust_evidence(&[]),
            TrustClassification::NoKnownContext
        );
    }

    #[test]
    fn the_strongest_evidence_item_classifies_a_signer() {
        let items = vec![
            evidence(
                TrustEvidenceKind::SocialActivity,
                EvidenceClassification::SociallyCorroborated,
                EvidenceScope::Current,
            ),
            evidence(
                TrustEvidenceKind::MaintainerRequest,
                EvidenceClassification::MaintainerDirected,
                EvidenceScope::Current,
            ),
            evidence(
                TrustEvidenceKind::RepositoryDomain,
                EvidenceClassification::OperationallyAssociated,
                EvidenceScope::Current,
            ),
        ];
        assert_eq!(
            classify_trust_evidence(&items),
            TrustClassification::MaintainerDirected
        );
    }

    #[test]
    fn an_unknown_signer_settles_with_the_state_coverage() {
        let state = settled_state(Vec::new(), Coverage::Partial);
        let resolution = trust_resolution(Some(&state), &Keys::generate().public_key());
        assert_eq!(
            resolution,
            settled_trust_resolution(Vec::new(), Coverage::Partial)
        );
    }

    #[test]
    fn a_missing_or_loading_state_is_loading() {
        assert_eq!(
            trust_resolution(None, &Keys::generate().public_key()),
            TrustResolution::Loading
        );
        assert_eq!(
            trust_resolution(
                Some(&TrustContextState::Loading),
                &Keys::generate().public_key()
            ),
            TrustResolution::Loading
        );
    }

    #[test]
    fn a_manual_quote_from_a_maintainer_adds_run_scoped_evidence() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "manual-trigger");
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[validated(&run)],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::MaintainerDirected)
        );
        let item = &resolution.evidence()[0];
        assert_eq!(item.summary, "Requested by a maintainer");
        assert_eq!(
            item.detail,
            "A confirmed repository maintainer manually requested this workflow run."
        );
        assert_eq!(item.scope, EvidenceScope::Run);
        assert_eq!(item.authors, vec![maintainer.public_key()]);
    }

    #[test]
    fn a_quote_from_a_non_maintainer_is_not_maintainer_direction() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let stranger = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &stranger, "manual-trigger");
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[Keys::generate().public_key()],
            &[],
            &[perspective(&owner)],
            &[validated(&run)],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext)
        );
    }

    #[test]
    fn a_service_quote_from_a_maintainer_reports_run_coverage() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "service-request");
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[validated(&run)],
        );
        let item = &resolution.evidence()[0];
        assert_eq!(item.summary, "Covered by a maintainer request");
        assert_eq!(
            item.detail,
            "A confirmed repository maintainer's service request was active when this workflow run started."
        );
        assert_eq!(item.authors, vec![maintainer.public_key()]);
    }

    #[test]
    fn a_quoted_request_signed_after_the_run_is_not_maintainer_direction() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // The run was handed off at 100; the request it quotes is signed at
        // 200, so freezing the quote is the coordinator backdating a request
        // the maintainer had not yet made.
        let run = run_with_quote(&coordinator, &owner, &maintainer, "service-request");
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[validated_at(&run, 200)],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert!(resolution.evidence().is_empty());
    }

    #[test]
    fn a_quoted_request_stopped_before_the_run_is_not_maintainer_direction() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "service-request");
        let state = settled_state(Vec::new(), Coverage::Complete);
        // Requested at 50, stopped at 80, and the run started at 100.
        let controls = vec![control(&maintainer, &coordinator, &owner, false, 80, 2)];

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &controls,
            &[perspective(&owner)],
            &[validated_at(&run, 50)],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext),
            "a quote cannot revive a request that was stopped before the run"
        );

        // The same request, with nothing closing it, still covers the run.
        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[validated_at(&run, 50)],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::MaintainerDirected)
        );
    }

    #[test]
    fn a_quoted_manual_trigger_signed_after_the_run_is_not_maintainer_direction() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "manual-trigger");
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[validated_at(&run, 200)],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext),
            "a trigger signed after the handoff authorized nothing here"
        );
    }

    #[test]
    fn a_current_standing_request_does_not_cover_an_earlier_run() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // started_at is 100; the request is signed afterwards.
        let run = run_from(&[workflow_result(&coordinator, &owner, "run-1", 100)]);
        let controls = vec![control(&maintainer, &coordinator, &owner, true, 200, 1)];
        let state = settled_state(
            vec![(
                coordinator.public_key(),
                settled_trust_resolution(
                    relationship_evidence(Some(&CoordinatorRelationship {
                        level: CoordinatorRelationshipLevel::Requested,
                        manual_run_count: 0,
                        service_run_count: 0,
                        requester_pubkeys: vec![maintainer.public_key()],
                    })),
                    Coverage::Complete,
                ),
            )],
            Coverage::Complete,
        );

        // The identity is maintainer-directed now...
        assert_eq!(
            trust_resolution(Some(&state), &coordinator.public_key()).classification(),
            Some(TrustClassification::MaintainerDirected)
        );
        // ...but the earlier run keeps no maintainer direction.
        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &controls,
            &[perspective(&owner)],
            &[],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext)
        );
    }

    #[test]
    fn control_history_covers_a_run_that_started_while_requested() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_from(&[workflow_result(&coordinator, &owner, "run-1", 100)]);
        let controls = vec![
            control(&maintainer, &coordinator, &owner, true, 50, 1),
            control(&maintainer, &coordinator, &owner, false, 300, 2),
        ];
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &controls,
            &[perspective(&owner)],
            &[],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::MaintainerDirected)
        );
        assert_eq!(
            resolution.evidence()[0].summary,
            "Covered by a maintainer request"
        );
        assert!(resolution.evidence()[0].authors.is_empty());
    }

    #[test]
    fn run_resolution_is_loading_while_the_state_is() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let run = run_from(&[workflow_result(&coordinator, &owner, "run-1", 100)]);
        assert_eq!(
            run_trust_resolution(None, &run, &[], &[], &[], &[]),
            TrustResolution::Loading
        );
    }

    fn delegated_run(coordinator: &Keys, provider: &Keys, owner: &Keys) -> WorkflowRun {
        let job = job_result(provider, coordinator, owner, "run-1", "build", 150);
        let base = workflow_result(coordinator, owner, "run-1", 200);
        let quoting = with_tags(coordinator, &base, |tags| {
            tags.push(tag(&[
                "q",
                &job.id.to_hex(),
                "wss://relay.example",
                &provider.public_key().to_hex(),
                "build",
            ]));
        });
        run_from(&[job, quoting])
    }

    #[test]
    fn coordinator_trust_reaches_the_provider_downgraded_and_job_scoped() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let run = delegated_run(&coordinator, &provider, &owner);
        let state = settled_state(
            vec![(
                coordinator.public_key(),
                settled_trust_resolution(
                    vec![evidence(
                        TrustEvidenceKind::MaintainerRequest,
                        EvidenceClassification::MaintainerDirected,
                        EvidenceScope::Current,
                    )],
                    Coverage::Complete,
                ),
            )],
            Coverage::Complete,
        );

        let resolution = job_trust_resolution(Some(&state), &run, &run.jobs[0]);
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::OperationallyAssociated)
        );
        let item = resolution.evidence().last().unwrap();
        assert_eq!(item.kind, TrustEvidenceKind::CoordinatorDelegation);
        assert_eq!(item.summary, "Accepted by the coordinator");
        assert_eq!(
            item.detail,
            "The independently contextual coordinator signed a Workflow Result accepting this provider's Job Result. This association is scoped to this job."
        );
        assert_eq!(item.scope, EvidenceScope::Run);
    }

    #[test]
    fn a_socially_corroborated_coordinator_delegates_social_corroboration() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let run = delegated_run(&coordinator, &provider, &owner);
        let state = settled_state(
            vec![(
                coordinator.public_key(),
                settled_trust_resolution(
                    vec![evidence(
                        TrustEvidenceKind::SocialActivity,
                        EvidenceClassification::SociallyCorroborated,
                        EvidenceScope::Current,
                    )],
                    Coverage::Complete,
                ),
            )],
            Coverage::Complete,
        );

        let resolution = job_trust_resolution(Some(&state), &run, &run.jobs[0]);
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::SociallyCorroborated)
        );
        assert_eq!(
            resolution.evidence().last().unwrap().summary,
            "Accepted by a socially corroborated coordinator"
        );
    }

    #[test]
    fn a_coordinator_without_context_contributes_nothing_to_the_provider() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let run = delegated_run(&coordinator, &provider, &owner);
        let state = settled_state(Vec::new(), Coverage::Complete);

        let resolution = job_trust_resolution(Some(&state), &run, &run.jobs[0]);
        assert_eq!(
            resolution,
            settled_trust_resolution(Vec::new(), Coverage::Complete)
        );
    }

    #[test]
    fn provider_evidence_does_not_flow_back_to_the_coordinator() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let run = delegated_run(&coordinator, &provider, &owner);
        let state = settled_state(
            vec![(
                provider.public_key(),
                settled_trust_resolution(
                    vec![evidence(
                        TrustEvidenceKind::RepositoryDomain,
                        EvidenceClassification::OperationallyAssociated,
                        EvidenceScope::Current,
                    )],
                    Coverage::Complete,
                ),
            )],
            Coverage::Complete,
        );

        assert_eq!(
            trust_resolution(Some(&state), &coordinator.public_key()).classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert_eq!(
            run_trust_resolution(Some(&state), &run, &[], &[], &[perspective(&owner)], &[])
                .classification(),
            Some(TrustClassification::NoKnownContext)
        );
    }

    #[test]
    fn an_unquoted_job_receives_no_delegated_evidence() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let job = job_result(&provider, &coordinator, &owner, "run-1", "build", 150);
        let run = run_from(&[job, workflow_result(&coordinator, &owner, "run-1", 200)]);
        let state = settled_state(
            vec![(
                coordinator.public_key(),
                settled_trust_resolution(
                    vec![evidence(
                        TrustEvidenceKind::MaintainerRequest,
                        EvidenceClassification::MaintainerDirected,
                        EvidenceScope::Current,
                    )],
                    Coverage::Complete,
                ),
            )],
            Coverage::Complete,
        );

        let resolution = job_trust_resolution(Some(&state), &run, &run.jobs[0]);
        assert!(resolution.evidence().is_empty());
    }

    #[test]
    fn job_coverage_is_partial_when_either_side_is_partial() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let run = delegated_run(&coordinator, &provider, &owner);
        let state = settled_state(
            vec![
                (
                    coordinator.public_key(),
                    settled_trust_resolution(
                        vec![evidence(
                            TrustEvidenceKind::RepositoryDomain,
                            EvidenceClassification::OperationallyAssociated,
                            EvidenceScope::Current,
                        )],
                        Coverage::Partial,
                    ),
                ),
                (
                    provider.public_key(),
                    settled_trust_resolution(Vec::new(), Coverage::Complete),
                ),
            ],
            Coverage::Complete,
        );

        assert_eq!(
            job_trust_resolution(Some(&state), &run, &run.jobs[0]).coverage(),
            Some(Coverage::Partial)
        );
    }

    #[test]
    fn a_rollup_reports_the_weakest_settled_run() {
        let rollup = summarize_run_trust(&[
            settled_trust_resolution(
                vec![evidence(
                    TrustEvidenceKind::MaintainerRequest,
                    EvidenceClassification::MaintainerDirected,
                    EvidenceScope::Run,
                )],
                Coverage::Complete,
            ),
            settled_trust_resolution(Vec::new(), Coverage::Complete),
            settled_trust_resolution(
                vec![evidence(
                    TrustEvidenceKind::RepositoryDomain,
                    EvidenceClassification::OperationallyAssociated,
                    EvidenceScope::Current,
                )],
                Coverage::Complete,
            ),
        ]);
        assert_eq!(
            rollup.classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert!(rollup.evidence().is_empty());
    }

    #[test]
    fn a_loading_run_makes_the_rollup_loading() {
        let rollup = summarize_run_trust(&[
            settled_trust_resolution(Vec::new(), Coverage::Complete),
            TrustResolution::Loading,
        ]);
        assert_eq!(rollup, TrustResolution::Loading);
    }

    #[test]
    fn any_partial_run_makes_the_rollup_partial() {
        let rollup = summarize_run_trust(&[
            settled_trust_resolution(Vec::new(), Coverage::Complete),
            settled_trust_resolution(
                vec![evidence(
                    TrustEvidenceKind::RepositoryDomain,
                    EvidenceClassification::OperationallyAssociated,
                    EvidenceScope::Current,
                )],
                Coverage::Partial,
            ),
        ]);
        assert_eq!(rollup.coverage(), Some(Coverage::Partial));
    }

    #[test]
    fn an_empty_rollup_settles_with_no_known_context() {
        assert_eq!(
            summarize_run_trust(&[]),
            settled_trust_resolution(Vec::new(), Coverage::Complete)
        );
    }

    #[test]
    fn relationship_evidence_uses_the_canonical_wording() {
        let maintainer = Keys::generate().public_key();
        let requested = relationship_evidence(Some(&CoordinatorRelationship {
            level: CoordinatorRelationshipLevel::Requested,
            manual_run_count: 0,
            service_run_count: 0,
            requester_pubkeys: vec![maintainer],
        }));
        assert_eq!(requested[0].summary, "Requested by repository maintainers");
        assert_eq!(
            requested[0].detail,
            "A confirmed repository maintainer currently asks this coordinator to run CI for this repository."
        );
        assert_eq!(requested[0].scope, EvidenceScope::Current);

        let one_manual = relationship_evidence(Some(&CoordinatorRelationship {
            level: CoordinatorRelationshipLevel::PreviouslyRequested,
            manual_run_count: 1,
            service_run_count: 0,
            requester_pubkeys: vec![maintainer],
        }));
        assert_eq!(one_manual[0].summary, "Earlier maintainer direction");
        assert_eq!(
            one_manual[0].detail,
            "A confirmed repository maintainer manually requested 1 run from this coordinator. This does not establish a standing service request."
        );

        let manual_and_service = relationship_evidence(Some(&CoordinatorRelationship {
            level: CoordinatorRelationshipLevel::PreviouslyRequested,
            manual_run_count: 2,
            service_run_count: 1,
            requester_pubkeys: vec![maintainer],
        }));
        assert_eq!(
            manual_and_service[0].detail,
            "A confirmed repository maintainer manually requested 2 runs from this coordinator. Earlier runs also cite a maintainer service request."
        );

        let service_only = relationship_evidence(Some(&CoordinatorRelationship {
            level: CoordinatorRelationshipLevel::PreviouslyRequested,
            manual_run_count: 0,
            service_run_count: 3,
            requester_pubkeys: vec![maintainer],
        }));
        assert_eq!(
            service_only[0].detail,
            "Earlier runs cite a maintainer service request, but there is no active standing request now."
        );

        let stopped = relationship_evidence(Some(&CoordinatorRelationship::default()));
        assert!(stopped.is_empty());
        assert!(relationship_evidence(None).is_empty());
    }

    #[test]
    fn classification_copy_matches_the_canonical_labels() {
        assert_eq!(
            TrustClassification::MaintainerDirected.label(),
            "Maintainer-directed"
        );
        assert_eq!(
            TrustClassification::OperationallyAssociated.label(),
            "Operationally associated"
        );
        assert_eq!(
            TrustClassification::SociallyCorroborated.label(),
            "Socially corroborated"
        );
        assert_eq!(
            TrustClassification::NoKnownContext.label(),
            "No known context"
        );
        assert_eq!(CONTEXT_INCOMPLETE_LABEL, "Context incomplete");
    }

    #[test]
    fn run_evidence_replaces_identity_level_current_requests() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = run_with_quote(&coordinator, &owner, &maintainer, "manual-trigger");
        let state = settled_state(
            vec![(
                coordinator.public_key(),
                settled_trust_resolution(
                    vec![
                        evidence(
                            TrustEvidenceKind::MaintainerRequest,
                            EvidenceClassification::MaintainerDirected,
                            EvidenceScope::Current,
                        ),
                        evidence(
                            TrustEvidenceKind::RepositoryDomain,
                            EvidenceClassification::OperationallyAssociated,
                            EvidenceScope::Current,
                        ),
                    ],
                    Coverage::Complete,
                ),
            )],
            Coverage::Complete,
        );

        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &[],
            &[perspective(&owner)],
            &[validated(&run)],
        );
        let kinds: Vec<TrustEvidenceKind> =
            resolution.evidence().iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TrustEvidenceKind::MaintainerRequest,
                TrustEvidenceKind::RepositoryDomain
            ]
        );
        assert_eq!(resolution.evidence()[0].scope, EvidenceScope::Run);
    }

    #[test]
    fn coverage_time_drives_control_reduction() {
        // A run with no timestamps has indeterminate coverage, so control
        // history cannot manufacture maintainer direction for it.
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let base = workflow_result(&coordinator, &owner, "run-1", 100);
        let without_times = with_tags(&coordinator, &base, |tags| {
            tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("started_at"));
        });
        let run = run_from(&[without_times]);
        assert_eq!(run.coverage_time(), None);

        let controls = vec![control(&maintainer, &coordinator, &owner, true, 1, 1)];
        let state = settled_state(Vec::new(), Coverage::Complete);
        let resolution = run_trust_resolution(
            Some(&state),
            &run,
            &[maintainer.public_key()],
            &controls,
            &[perspective(&owner)],
            &[],
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert_eq!(
            was_service_requested_when_run_started(
                &run,
                &controls,
                &[maintainer.public_key()],
                &[perspective(&owner)]
            ),
            None
        );
    }
}
