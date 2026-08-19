//! Service Request / Stop reduction and coordinator relationship tiers.
//!
//! Controls (kind 9843 Requests and kind 9844 Stops addressed to a
//! coordinator for one repository perspective) are immutable and totally
//! ordered: a greater `created_at` is later, and at equal timestamps the
//! lexicographically lower event id is later.
//!
//! Coverage at a time `T` considers only controls at or before `T`. A Stop
//! from a confirmed maintainer closes every earlier Request for the
//! perspective; a Stop from any other author closes only that author's own
//! earlier Requests, which is what a removed maintainer's Stop retains.
//! Service was requested at `T` when an accepted Request — one authored by a
//! confirmed maintainer — remains unclosed there.
//!
//! A current standing Request therefore never retroactively covers a run that
//! started before it. A Request a run *quotes* is reduced the same way: the
//! coordinator chose when to freeze the quote, so the quote decides which
//! request is being cited, never when it applied.

use std::collections::{HashMap, HashSet};

use nostr::prelude::{Coordinate, PublicKey, Timestamp};

use super::{
    events::WorkflowRun,
    kinds::ServiceControl,
    provenance::{ValidatedProvenance, ValidatedRequest},
    total_order_position,
};

/// How a repository relates to a coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorRelationshipLevel {
    /// A confirmed maintainer's Service Request is standing now.
    Requested,
    /// Maintainer direction exists in the history, but not now.
    PreviouslyRequested,
    Unassociated,
}

/// Repository-to-coordinator relationship, with the runs and signers behind
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorRelationship {
    pub level: CoordinatorRelationshipLevel,
    pub manual_run_count: usize,
    pub service_run_count: usize,
    pub requester_pubkeys: Vec<PublicKey>,
}

impl Default for CoordinatorRelationship {
    fn default() -> Self {
        Self {
            level: CoordinatorRelationshipLevel::Unassociated,
            manual_run_count: 0,
            service_run_count: 0,
            requester_pubkeys: Vec::new(),
        }
    }
}

/// A run's frozen maintainer provenance, when its quote names a confirmed
/// maintainer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMaintainerLink {
    /// The run replays a maintainer's Manual Trigger.
    Manual,
    /// The run cites a maintainer's Service Request.
    Service,
}

/// Classify a run's frozen provenance quote, when it has been validated.
///
/// A coordinator-authored `q` tag naming a maintainer pubkey is the
/// coordinator's own claim, not evidence. `validated_provenance` holds the
/// verdicts reached by fetching and checking quoted requests — event id,
/// kind, author, repository, coordinator and run context — so a link is only
/// reported when a verdict covers *this* run and its requester is a confirmed
/// maintainer. A verdict reached for another run is not evidence here, even
/// when both runs quote the same request: the check that passed there was
/// against that run's workflow, commit and pull-request context. Callers with
/// no validation available pass an empty slice and get `None`, which is the
/// safe direction: the control-history reduction remains available as
/// independent evidence.
///
/// A validated quote is then placed in time by the same rules an unquoted
/// control obeys, evaluated at the run's handoff: a Service Request signed
/// after the run, or closed by a Stop before it, covers nothing, and a Manual
/// Trigger must precede the run it authorized. Freezing a `q` tag is the
/// coordinator's act, so without this a coordinator could quote a request
/// that had already been withdrawn — or one a maintainer signed for later
/// work — and present an old run as maintainer-directed.
#[must_use]
pub fn run_maintainer_link(
    run: &WorkflowRun,
    confirmed_maintainers: &[PublicKey],
    controls: &[ServiceControl],
    validated_provenance: &[ValidatedProvenance],
) -> Option<RunMaintainerLink> {
    let quote = run.provenance()?;
    let validated = validated_provenance
        .iter()
        .find(|validated| validated.covers(run))?;
    if !confirmed_maintainers.contains(&quote.requester) {
        return None;
    }
    // No handoff time means coverage is indeterminate, exactly as it is for
    // the reduction: it is never assumed.
    let handoff = run.coverage_time()?;
    match &validated.request {
        ValidatedRequest::ManualTrigger { created_at, .. } => {
            (*created_at <= handoff).then_some(RunMaintainerLink::Manual)
        }
        ValidatedRequest::ServiceRequest(request) => {
            was_quoted_request_open_at(request, controls, confirmed_maintainers, handoff)
                .then_some(RunMaintainerLink::Service)
        }
    }
}

/// Whether the Service Request a run quotes was open at `at`.
///
/// The quoted request is reduced together with the rest of its perspective's
/// control history, so it obeys the rules every other request obeys: one
/// signed after `at` is not in the history yet, and one a Stop closed before
/// `at` is no longer open. The perspective is the request's own — provenance
/// validation has already placed it inside the maintainer closure — so a
/// quote cannot escape the history by naming a coordinate the caller did not
/// ask about.
fn was_quoted_request_open_at(
    request: &ServiceControl,
    controls: &[ServiceControl],
    confirmed_maintainers: &[PublicKey],
    at: Timestamp,
) -> bool {
    let mut history: Vec<ServiceControl> = controls.to_vec();
    if !history
        .iter()
        .any(|control| control.event_id == request.event_id)
    {
        history.push(request.clone());
    }
    open_requests(
        &history,
        &request.coordinator,
        &request.repository.coordinate,
        confirmed_maintainers,
        at,
    )
    .iter()
    .any(|open| open.event_id == request.event_id)
}

/// The pubkey the run's frozen quote names as the requester.
#[must_use]
pub fn run_requester(run: &WorkflowRun) -> Option<PublicKey> {
    run.provenance().map(|quote| quote.requester)
}

/// Reduce the control history for one perspective and return the Requests
/// still open at `at`.
fn open_requests<'a>(
    controls: &'a [ServiceControl],
    coordinator: &PublicKey,
    perspective: &Coordinate,
    confirmed_maintainers: &[PublicKey],
    at: Timestamp,
) -> Vec<&'a ServiceControl> {
    let mut relevant: Vec<&ServiceControl> = controls
        .iter()
        .filter(|control| {
            control.coordinator == *coordinator
                && control.repository.coordinate == *perspective
                && control.created_at <= at
        })
        .collect();
    relevant.sort_by_key(|control| total_order_position(control.created_at, control.event_id));

    let mut open: Vec<&ServiceControl> = Vec::new();
    for control in relevant {
        if control.is_request {
            open.push(control);
        } else if confirmed_maintainers.contains(&control.author) {
            // A maintainer's Stop closes the whole perspective.
            open.clear();
        } else {
            // Any other author — including a removed maintainer — closes only
            // their own Requests.
            open.retain(|request| request.author != control.author);
        }
    }
    open
}

/// Whether an accepted Service Request was open at `at` for any of the given
/// repository perspectives.
///
/// The acceptance policy is the confirmed-maintainer list: an operator may
/// accept other requester pubkeys, but ngit does not treat those as
/// maintainer direction.
#[must_use]
pub fn was_service_requested_at(
    controls: &[ServiceControl],
    coordinator: &PublicKey,
    perspectives: &[Coordinate],
    confirmed_maintainers: &[PublicKey],
    at: Timestamp,
) -> bool {
    perspectives.iter().any(|perspective| {
        open_requests(
            controls,
            coordinator,
            perspective,
            confirmed_maintainers,
            at,
        )
        .iter()
        .any(|request| confirmed_maintainers.contains(&request.author))
    })
}

/// Whether a standing Service Request was active when the run was handed off.
///
/// `None` means the run carries neither `started_at` nor `queued_at`, so
/// coverage is indeterminate — it is never assumed.
#[must_use]
pub fn was_service_requested_when_run_started(
    run: &WorkflowRun,
    controls: &[ServiceControl],
    confirmed_maintainers: &[PublicKey],
    perspectives: &[Coordinate],
) -> Option<bool> {
    let started_at = run.coverage_time()?;
    Some(was_service_requested_at(
        controls,
        &run.coordinator,
        perspectives,
        confirmed_maintainers,
        started_at,
    ))
}

/// Coordinators with an accepted, unclosed Service Request at `at`.
#[must_use]
pub fn requested_coordinators(
    controls: &[ServiceControl],
    perspectives: &[Coordinate],
    confirmed_maintainers: &[PublicKey],
    at: Timestamp,
) -> HashSet<PublicKey> {
    addressed_coordinators(controls, perspectives)
        .into_iter()
        .filter(|coordinator| {
            was_service_requested_at(
                controls,
                coordinator,
                perspectives,
                confirmed_maintainers,
                at,
            )
        })
        .collect()
}

/// Coordinators a confirmed maintainer requested at some point at or before
/// `at`, but whose request is not standing there.
#[must_use]
pub fn previously_requested_coordinators(
    controls: &[ServiceControl],
    perspectives: &[Coordinate],
    confirmed_maintainers: &[PublicKey],
    at: Timestamp,
) -> HashSet<PublicKey> {
    addressed_coordinators(controls, perspectives)
        .into_iter()
        .filter(|coordinator| {
            let ever_accepted = controls.iter().any(|control| {
                control.is_request
                    && control.coordinator == *coordinator
                    && control.created_at <= at
                    && confirmed_maintainers.contains(&control.author)
                    && perspectives.contains(&control.repository.coordinate)
            });
            ever_accepted
                && !was_service_requested_at(
                    controls,
                    coordinator,
                    perspectives,
                    confirmed_maintainers,
                    at,
                )
        })
        .collect()
}

fn addressed_coordinators(
    controls: &[ServiceControl],
    perspectives: &[Coordinate],
) -> HashSet<PublicKey> {
    controls
        .iter()
        .filter(|control| perspectives.contains(&control.repository.coordinate))
        .map(|control| control.coordinator)
        .collect()
}

/// Derive repository-to-coordinator relationship tiers.
///
/// A current standing Service Request is strongest. Historical controls or
/// per-run request provenance establish a previous relationship, while every
/// individual run retains its own frozen provenance.
#[must_use]
pub fn classify_coordinator_relationships(
    runs: &[WorkflowRun],
    confirmed_maintainers: &[PublicKey],
    requested_coordinator_pubkeys: &HashSet<PublicKey>,
    previously_requested_coordinator_pubkeys: &HashSet<PublicKey>,
    controls: &[ServiceControl],
    validated_provenance: &[ValidatedProvenance],
) -> HashMap<PublicKey, CoordinatorRelationship> {
    let mut relationships: HashMap<PublicKey, CoordinatorRelationship> = HashMap::new();

    for run in runs {
        let entry = relationships.entry(run.coordinator).or_default();
        if let Some(link) =
            run_maintainer_link(run, confirmed_maintainers, controls, validated_provenance)
        {
            entry.level = CoordinatorRelationshipLevel::PreviouslyRequested;
            match link {
                RunMaintainerLink::Manual => entry.manual_run_count += 1,
                RunMaintainerLink::Service => entry.service_run_count += 1,
            }
            if let Some(requester) = run_requester(run) {
                if !entry.requester_pubkeys.contains(&requester) {
                    entry.requester_pubkeys.push(requester);
                }
            }
        }
    }

    for control in controls {
        if !control.is_request || !confirmed_maintainers.contains(&control.author) {
            continue;
        }
        let entry = relationships.entry(control.coordinator).or_default();
        if !entry.requester_pubkeys.contains(&control.author) {
            entry.requester_pubkeys.push(control.author);
        }
    }

    for pubkey in previously_requested_coordinator_pubkeys {
        relationships.entry(*pubkey).or_default().level =
            CoordinatorRelationshipLevel::PreviouslyRequested;
    }

    for pubkey in requested_coordinator_pubkeys {
        relationships.entry(*pubkey).or_default().level = CoordinatorRelationshipLevel::Requested;
    }

    relationships
}

/// The relationship for one coordinator, defaulting to unassociated.
#[must_use]
pub fn coordinator_relationship(
    relationships: &HashMap<PublicKey, CoordinatorRelationship>,
    pubkey: &PublicKey,
) -> CoordinatorRelationship {
    relationships.get(pubkey).cloned().unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use nostr::prelude::{EventId, Keys, Kind};

    use super::{
        super::{
            events::group_workflow_runs,
            kinds::{
                MARKER_MANUAL_TRIGGER, MARKER_SERVICE_REQUEST, ProvenanceKind, RepoReference,
                test_events::*,
            },
        },
        *,
    };

    fn perspective(owner: &Keys) -> Coordinate {
        Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: owner.public_key(),
            identifier: "ngit".to_owned(),
        }
    }

    /// Build a control directly so a test can pin its event id, which the
    /// total order uses as the tie-break at equal timestamps.
    pub(crate) fn control(
        author: &Keys,
        coordinator: &Keys,
        owner: &Keys,
        is_request: bool,
        created_at: u64,
        id_byte: u8,
    ) -> ServiceControl {
        ServiceControl {
            event_id: EventId::from_slice(&[id_byte; 32]).unwrap(),
            author: author.public_key(),
            created_at: Timestamp::from_secs(created_at),
            coordinator: coordinator.public_key(),
            repository: RepoReference {
                coordinate: perspective(owner),
                relay_hint: None,
            },
            is_request,
        }
    }

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_secs(secs)
    }

    /// A run whose Workflow Result carries a frozen provenance quote.
    pub(crate) fn quoted_run(
        coordinator: &Keys,
        owner: &Keys,
        requester: &Keys,
        run_id: &str,
        quote_id: EventId,
        marker: &str,
    ) -> WorkflowRun {
        let base = workflow_result(coordinator, owner, run_id, 100);
        let quoting = with_tags(coordinator, &base, |tags| {
            tags.push(tag(&[
                "q",
                &quote_id.to_hex(),
                "wss://relay.example",
                &requester.public_key().to_hex(),
                marker,
            ]));
        });
        let grouped = group_workflow_runs(std::slice::from_ref(&quoting));
        assert!(grouped.skipped.is_empty(), "{:?}", grouped.skipped);
        grouped.runs.into_iter().next().expect("one run")
    }

    fn quote_id(byte: u8) -> EventId {
        EventId::from_slice(&[byte; 32]).unwrap()
    }

    /// The verdict `validate_run_quotes` reaches for a run whose quote held,
    /// with the quoted request signed before the run was handed off.
    pub(crate) fn validated(run: &WorkflowRun) -> ValidatedProvenance {
        let signed_before = run
            .coverage_time()
            .map_or(0, |handoff| handoff.as_secs().saturating_sub(50));
        validated_at(run, signed_before)
    }

    /// The same verdict, with the quoted request signed at `created_at`.
    pub(crate) fn validated_at(run: &WorkflowRun, created_at: u64) -> ValidatedProvenance {
        let quote = run.provenance().expect("a quoted run");
        let request = match quote.kind {
            ProvenanceKind::ManualTrigger => ValidatedRequest::ManualTrigger {
                event_id: quote.event_id,
                created_at: ts(created_at),
            },
            ProvenanceKind::ServiceRequest => {
                ValidatedRequest::ServiceRequest(Box::new(ServiceControl {
                    event_id: quote.event_id,
                    author: quote.requester,
                    created_at: ts(created_at),
                    coordinator: run.coordinator,
                    repository: run
                        .repositories
                        .first()
                        .expect("a run names its repository")
                        .clone(),
                    is_request: true,
                }))
            }
        };
        ValidatedProvenance::for_run(run, request)
    }

    #[test]
    fn validated_run_provenance_counts_toward_the_previous_relationship() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let manual = quote_id(0xa1);
        let service = quote_id(0xb2);
        let runs = vec![
            quoted_run(
                &coordinator,
                &owner,
                &maintainer,
                "run-1",
                manual,
                MARKER_MANUAL_TRIGGER,
            ),
            quoted_run(
                &coordinator,
                &owner,
                &maintainer,
                "run-2",
                service,
                MARKER_SERVICE_REQUEST,
            ),
        ];

        let validated_provenance = vec![validated(&runs[0]), validated(&runs[1])];
        let relationships = classify_coordinator_relationships(
            &runs,
            &[maintainer.public_key()],
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &validated_provenance,
        );
        let relationship = coordinator_relationship(&relationships, &coordinator.public_key());
        assert_eq!(
            relationship.level,
            CoordinatorRelationshipLevel::PreviouslyRequested
        );
        assert_eq!(relationship.manual_run_count, 1);
        assert_eq!(relationship.service_run_count, 1);
        assert_eq!(
            relationship.requester_pubkeys,
            vec![maintainer.public_key()],
            "one requester, deduplicated across runs"
        );
    }

    #[test]
    fn unvalidated_run_provenance_establishes_no_relationship() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let runs = vec![quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            quote_id(0xa1),
            MARKER_MANUAL_TRIGGER,
        )];

        assert_eq!(
            run_maintainer_link(&runs[0], &[maintainer.public_key()], &[], &[]),
            None
        );
        let relationships = classify_coordinator_relationships(
            &runs,
            &[maintainer.public_key()],
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &[],
        );
        let relationship = coordinator_relationship(&relationships, &coordinator.public_key());
        assert_eq!(
            relationship.level,
            CoordinatorRelationshipLevel::Unassociated
        );
        assert_eq!(relationship.manual_run_count, 0);
        assert!(relationship.requester_pubkeys.is_empty());
    }

    #[test]
    fn a_verdict_reached_for_one_run_does_not_link_another_quoting_it() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let quote = quote_id(0xa1);
        // Two runs quote the same trigger; validation held for the first only.
        let runs = vec![
            quoted_run(
                &coordinator,
                &owner,
                &maintainer,
                "run-1",
                quote,
                MARKER_MANUAL_TRIGGER,
            ),
            quoted_run(
                &coordinator,
                &owner,
                &maintainer,
                "run-2",
                quote,
                MARKER_MANUAL_TRIGGER,
            ),
        ];
        let maintainers = [maintainer.public_key()];
        let validated_provenance = vec![validated(&runs[0])];

        assert_eq!(
            run_maintainer_link(&runs[0], &maintainers, &[], &validated_provenance),
            Some(RunMaintainerLink::Manual)
        );
        assert_eq!(
            run_maintainer_link(&runs[1], &maintainers, &[], &validated_provenance),
            None,
            "the quoted id alone is not the second run's evidence"
        );

        let relationships = classify_coordinator_relationships(
            &runs,
            &maintainers,
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &validated_provenance,
        );
        assert_eq!(
            coordinator_relationship(&relationships, &coordinator.public_key()).manual_run_count,
            1,
            "only the validated run counts"
        );
    }

    #[test]
    fn a_quoted_request_signed_after_the_run_is_not_coverage() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // The run was handed off at 100.
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            quote_id(0xa1),
            MARKER_SERVICE_REQUEST,
        );
        let maintainers = [maintainer.public_key()];

        assert_eq!(
            run_maintainer_link(&run, &maintainers, &[], &[validated_at(&run, 200)]),
            None,
            "a request signed after the run cannot have covered it"
        );
        assert_eq!(
            run_maintainer_link(&run, &maintainers, &[], &[validated_at(&run, 100)]),
            Some(RunMaintainerLink::Service),
            "a request signed at the handoff is in the history there"
        );
    }

    #[test]
    fn a_quoted_request_stopped_before_the_run_is_not_coverage() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            quote_id(0xa1),
            MARKER_SERVICE_REQUEST,
        );
        let maintainers = [maintainer.public_key()];
        let validated_provenance = [validated_at(&run, 50)];

        let stopped_first = vec![control(&maintainer, &coordinator, &owner, false, 80, 2)];
        assert_eq!(
            run_maintainer_link(&run, &maintainers, &stopped_first, &validated_provenance),
            None,
            "the quoted request was already closed when the run started"
        );

        // The same Stop after the handoff leaves the run as it was: the
        // reduction is evaluated at the run, not now.
        let stopped_later = vec![control(&maintainer, &coordinator, &owner, false, 150, 2)];
        assert_eq!(
            run_maintainer_link(&run, &maintainers, &stopped_later, &validated_provenance),
            Some(RunMaintainerLink::Service)
        );
    }

    #[test]
    fn a_quoted_trigger_signed_after_the_run_is_not_coverage() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let run = quoted_run(
            &coordinator,
            &owner,
            &maintainer,
            "run-1",
            quote_id(0xa1),
            MARKER_MANUAL_TRIGGER,
        );
        let maintainers = [maintainer.public_key()];

        assert_eq!(
            run_maintainer_link(&run, &maintainers, &[], &[validated_at(&run, 200)]),
            None,
            "a trigger cannot authorize a run that was already under way"
        );
        assert_eq!(
            run_maintainer_link(&run, &maintainers, &[], &[validated_at(&run, 60)]),
            Some(RunMaintainerLink::Manual)
        );
    }

    #[test]
    fn an_accepted_request_covers_later_times() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![control(&maintainer, &coordinator, &owner, true, 100, 1)];
        let maintainers = [maintainer.public_key()];

        assert!(was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &[perspective(&owner)],
            &maintainers,
            ts(150),
        ));
    }

    #[test]
    fn a_request_does_not_retroactively_cover_an_earlier_run() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![control(&maintainer, &coordinator, &owner, true, 200, 1)];
        let maintainers = [maintainer.public_key()];

        assert!(!was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &[perspective(&owner)],
            &maintainers,
            ts(150),
        ));
    }

    #[test]
    fn a_later_stop_does_not_rewrite_an_earlier_covered_run() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![
            control(&maintainer, &coordinator, &owner, true, 100, 1),
            control(&maintainer, &coordinator, &owner, false, 300, 2),
        ];
        let maintainers = [maintainer.public_key()];
        let perspectives = [perspective(&owner)];

        assert!(was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &perspectives,
            &maintainers,
            ts(200),
        ));
        assert!(!was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &perspectives,
            &maintainers,
            ts(400),
        ));
    }

    #[test]
    fn at_equal_timestamps_the_lower_event_id_is_later() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainers = [maintainer.public_key()];
        let perspectives = [perspective(&owner)];

        // Stop has the lower id, so it is later than the Request.
        let stop_last = vec![
            control(&maintainer, &coordinator, &owner, true, 100, 9),
            control(&maintainer, &coordinator, &owner, false, 100, 1),
        ];
        assert!(!was_service_requested_at(
            &stop_last,
            &coordinator.public_key(),
            &perspectives,
            &maintainers,
            ts(100),
        ));

        // Request has the lower id, so it is later than the Stop.
        let request_last = vec![
            control(&maintainer, &coordinator, &owner, true, 100, 1),
            control(&maintainer, &coordinator, &owner, false, 100, 9),
        ];
        assert!(was_service_requested_at(
            &request_last,
            &coordinator.public_key(),
            &perspectives,
            &maintainers,
            ts(100),
        ));
    }

    #[test]
    fn a_maintainer_stop_closes_every_earlier_request() {
        let alice = Keys::generate();
        let bob = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![
            control(&alice, &coordinator, &owner, true, 100, 1),
            control(&bob, &coordinator, &owner, true, 110, 2),
            control(&bob, &coordinator, &owner, false, 200, 3),
        ];
        let maintainers = [alice.public_key(), bob.public_key()];

        assert!(!was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &[perspective(&owner)],
            &maintainers,
            ts(300),
        ));
    }

    #[test]
    fn a_non_maintainer_stop_closes_only_their_own_requests() {
        let maintainer = Keys::generate();
        let outsider = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![
            control(&maintainer, &coordinator, &owner, true, 100, 1),
            control(&outsider, &coordinator, &owner, true, 110, 2),
            control(&outsider, &coordinator, &owner, false, 200, 3),
        ];
        let maintainers = [maintainer.public_key()];

        assert!(was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &[perspective(&owner)],
            &maintainers,
            ts(300),
        ));
    }

    #[test]
    fn a_removed_maintainers_stop_retains_author_local_effect() {
        let maintainer = Keys::generate();
        let removed = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![
            control(&maintainer, &coordinator, &owner, true, 100, 1),
            control(&removed, &coordinator, &owner, true, 110, 2),
            control(&removed, &coordinator, &owner, false, 200, 3),
        ];
        // `removed` is no longer a confirmed maintainer, so their Stop cannot
        // close the perspective, and their own Request is no longer accepted.
        let maintainers = [maintainer.public_key()];
        let perspectives = [perspective(&owner)];

        assert!(was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &perspectives,
            &maintainers,
            ts(300),
        ));

        let only_removed = vec![
            control(&removed, &coordinator, &owner, true, 110, 2),
            control(&removed, &coordinator, &owner, false, 200, 3),
        ];
        assert!(!was_service_requested_at(
            &only_removed,
            &coordinator.public_key(),
            &perspectives,
            &maintainers,
            ts(300),
        ));
    }

    #[test]
    fn a_stop_is_scoped_to_its_own_perspective() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let other_owner = Keys::generate();
        let controls = vec![
            control(&maintainer, &coordinator, &owner, true, 100, 1),
            control(&maintainer, &coordinator, &other_owner, false, 200, 2),
        ];
        let maintainers = [maintainer.public_key()];

        assert!(was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &[perspective(&owner), perspective(&other_owner)],
            &maintainers,
            ts(300),
        ));
    }

    #[test]
    fn a_request_from_a_non_maintainer_is_not_accepted() {
        let outsider = Keys::generate();
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![control(&outsider, &coordinator, &owner, true, 100, 1)];

        assert!(!was_service_requested_at(
            &controls,
            &coordinator.public_key(),
            &[perspective(&owner)],
            &[maintainer.public_key()],
            ts(300),
        ));
    }

    #[test]
    fn controls_addressed_to_another_coordinator_are_ignored() {
        let maintainer = Keys::generate();
        let coordinator = Keys::generate();
        let other_coordinator = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![control(&maintainer, &coordinator, &owner, true, 100, 1)];

        assert!(!was_service_requested_at(
            &controls,
            &other_coordinator.public_key(),
            &[perspective(&owner)],
            &[maintainer.public_key()],
            ts(300),
        ));
    }

    #[test]
    fn coordinator_tiers_follow_the_control_history() {
        let maintainer = Keys::generate();
        let standing = Keys::generate();
        let stopped = Keys::generate();
        let owner = Keys::generate();
        let controls = vec![
            control(&maintainer, &standing, &owner, true, 100, 1),
            control(&maintainer, &stopped, &owner, true, 100, 2),
            control(&maintainer, &stopped, &owner, false, 200, 3),
        ];
        let maintainers = [maintainer.public_key()];
        let perspectives = [perspective(&owner)];

        let requested = requested_coordinators(&controls, &perspectives, &maintainers, ts(300));
        let previously =
            previously_requested_coordinators(&controls, &perspectives, &maintainers, ts(300));
        assert_eq!(
            requested,
            HashSet::from([standing.public_key()]),
            "only the standing request is current"
        );
        assert_eq!(previously, HashSet::from([stopped.public_key()]));

        let relationships = classify_coordinator_relationships(
            &[],
            &maintainers,
            &requested,
            &previously,
            &controls,
            &[],
        );
        assert_eq!(
            coordinator_relationship(&relationships, &standing.public_key()).level,
            CoordinatorRelationshipLevel::Requested
        );
        assert_eq!(
            coordinator_relationship(&relationships, &stopped.public_key()).level,
            CoordinatorRelationshipLevel::PreviouslyRequested
        );
        assert_eq!(
            coordinator_relationship(&relationships, &Keys::generate().public_key()).level,
            CoordinatorRelationshipLevel::Unassociated
        );
    }
}
