//! Assembling the CI trust context, in two tiers.
//!
//! Both tiers produce the same [`CiTrustContext`]; they differ only in which
//! evidence they are allowed to gather.
//!
//! - The **cache tier** ([`resolve_cache_tier`]) is pure and local: the
//!   control-history reduction, plus quote validation for the quoted requests
//!   the local nostr cache already holds. It performs no network work at all,
//!   so the NIP-05 domain ladder never runs and coverage is partial — the
//!   canonical [`CONTEXT_INCOMPLETE_LABEL`] caveat applies. `pr list` uses it.
//! - The **full tier** ([`resolve_full_tier`]) additionally obtains the missing
//!   quoted requests through an injected fetcher and verifies NIP-05 identities
//!   through an injected lookup. `pr view`, `pr merge` and `ci status` use it.
//!   Only the signers in [`identity_lookup_signers`] are resolved, and the
//!   lookups themselves are bounded in count and in total time, so a publisher
//!   cannot scale the wait by naming more domains.
//!
//! Neither tier can leave a signer classified as
//! [`TrustClassification::NoKnownContext`] while one of its queries is still
//! outstanding: a tier is called with the inputs it needs and returns a
//! settled context, and a caller whose own queries have not finished reports
//! [`CiTrustContext::loading`] instead. Anything that did not settle — a
//! failed fetch, a failed lookup, a quote that could not be retrieved — makes
//! coverage partial and never becomes a negative claim.
//!
//! Level 3 seen-in-your-network evidence is deferred. Its per-signer evidence
//! would be appended in [`assemble`], and its inputs (the people the viewer
//! follows and the CI activity around their repositories) would join
//! [`CiInputs`]; nothing else in the model needs to change.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use nostr::prelude::{Coordinate, Event, EventId, PublicKey, Timestamp};

use super::{
    controls::{
        CoordinatorRelationship, classify_coordinator_relationships, coordinator_relationship,
        previously_requested_coordinators, requested_coordinators,
    },
    domain::{
        Nip05Cache, Nip05Lookup, VerifiedIdentity, any_lookup_failed, domain_evidence,
        repository_grasp_domains, verify_identities,
    },
    events::WorkflowRun,
    kinds::{JobResult, ServiceControl},
    provenance::{self, RejectedProvenance, ValidatedProvenance, WantedQuote},
    trust::{
        CONTEXT_INCOMPLETE_LABEL, Coverage, TrustClassification, TrustContextState,
        TrustResolution, job_trust_resolution, relationship_evidence, run_trust_resolution,
        settled_trust_resolution, summarize_run_trust, trust_resolution,
    },
};
use crate::repo_ref::RepoRef;

/// The resolved repository, reduced to what trust evaluation needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryContext {
    /// `RepoRef::confirmed_maintainers()` — never invited maintainers.
    pub confirmed_maintainers: Vec<PublicKey>,
    /// Every confirmed member announcement coordinate. These are the
    /// perspectives control history is reduced over and the coordinates a
    /// quoted request must intersect.
    pub coordinates: Vec<Coordinate>,
    /// GRASP domains from the repository's clone URLs.
    pub grasp_domains: Vec<String>,
}

impl RepositoryContext {
    /// Derive the context from a resolved repository.
    #[must_use]
    pub fn from_repo_ref(repo_ref: &RepoRef) -> Self {
        let mut coordinates: Vec<Coordinate> = repo_ref
            .coordinates()
            .into_iter()
            .map(|coordinate| coordinate.coordinate)
            .collect();
        coordinates.sort_by(|a, b| {
            a.public_key
                .to_hex()
                .cmp(&b.public_key.to_hex())
                .then_with(|| a.identifier.cmp(&b.identifier))
        });
        Self {
            confirmed_maintainers: repo_ref.confirmed_maintainers(),
            coordinates,
            grasp_domains: repository_grasp_domains(&repo_ref.git_server),
        }
    }
}

/// Everything both tiers read, all of it already in hand.
#[derive(Debug, Clone)]
pub struct CiInputs<'a> {
    pub repository: &'a RepositoryContext,
    /// The runs being described.
    pub runs: &'a [WorkflowRun],
    /// Every Service Request/Stop known for the repository.
    pub controls: &'a [ServiceControl],
    /// Quoted 9843/9840 events the local cache already holds, by event id.
    pub quoted_events: &'a HashMap<EventId, Event>,
    /// The `nip05` value from each signer's kind-0 profile, where known.
    pub profile_nip05: &'a HashMap<PublicKey, String>,
    /// The time coverage is evaluated at: which Service Requests stand *now*.
    pub now: Timestamp,
    /// Coverage the caller's own queries settled with. A failed relay makes
    /// the whole context partial regardless of what this module finds.
    pub input_coverage: Coverage,
}

impl<'a> CiInputs<'a> {
    /// Inputs with complete caller-side coverage.
    #[must_use]
    pub fn new(
        repository: &'a RepositoryContext,
        runs: &'a [WorkflowRun],
        controls: &'a [ServiceControl],
        quoted_events: &'a HashMap<EventId, Event>,
        profile_nip05: &'a HashMap<PublicKey, String>,
        now: Timestamp,
    ) -> Self {
        Self {
            repository,
            runs,
            controls,
            quoted_events,
            profile_nip05,
            now,
            input_coverage: Coverage::Complete,
        }
    }
}

/// Obtains quoted requests a run references but the local cache lacks.
///
/// The CLI wires the real relay fetch; unit tests inject a stub. A failure is
/// reported as such and leaves coverage partial — it never fails the command.
#[async_trait]
pub trait QuotedEventFetcher {
    /// Fetch as many of `wanted` as can be found.
    ///
    /// An implementation need not verify what it returns: every quoted event
    /// has its id and signature checked in
    /// [`provenance::validate_run_provenance`], whatever its source. It also
    /// need not filter — an event that is not the one wanted is rejected
    /// there rather than trusted.
    ///
    /// # Errors
    ///
    /// Returns an error when the fetch could not be performed at all.
    async fn fetch(&self, wanted: &[WantedQuote]) -> Result<Vec<Event>>;
}

/// A fetcher that finds nothing, for the cache tier and for tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoQuotedEventFetcher;

#[async_trait]
impl QuotedEventFetcher for NoQuotedEventFetcher {
    async fn fetch(&self, _wanted: &[WantedQuote]) -> Result<Vec<Event>> {
        Ok(Vec::new())
    }
}

/// The assembled trust context for one repository view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTrustContext {
    /// Per-signer resolutions and the overall coverage.
    pub state: TrustContextState,
    /// Repository-to-coordinator relationship tiers.
    pub relationships: HashMap<PublicKey, CoordinatorRelationship>,
    // The provenance verdicts and the repository facts they were reached
    // from are private: a verdict only enters `validated_provenance` by
    // passing `provenance::validate_run_provenance` for the run it names, and
    // a caller that could push into it could manufacture maintainer direction
    // for any run.
    validated_provenance: Vec<ValidatedProvenance>,
    rejected_provenance: Vec<RejectedProvenance>,
    unavailable_provenance: Vec<WantedQuote>,
    confirmed_maintainers: Vec<PublicKey>,
    coordinates: Vec<Coordinate>,
    controls: Vec<ServiceControl>,
}

impl CiTrustContext {
    /// A context whose queries have not settled.
    ///
    /// Every resolution derived from it is [`TrustResolution::Loading`], so a
    /// caller can render "checking" rather than "no known context".
    #[must_use]
    pub fn loading() -> Self {
        Self {
            state: TrustContextState::Loading,
            relationships: HashMap::new(),
            validated_provenance: Vec::new(),
            rejected_provenance: Vec::new(),
            unavailable_provenance: Vec::new(),
            confirmed_maintainers: Vec::new(),
            coordinates: Vec::new(),
            controls: Vec::new(),
        }
    }

    /// Runs whose quote passed provenance validation, each verdict scoped to
    /// the run it was reached for.
    #[must_use]
    pub fn validated_provenance(&self) -> &[ValidatedProvenance] {
        &self.validated_provenance
    }

    /// Quotes that were checked and did not hold up, with the reason.
    #[must_use]
    pub fn rejected_provenance(&self) -> &[RejectedProvenance] {
        &self.rejected_provenance
    }

    /// Quotes nothing could retrieve, so nothing checked. These leave
    /// coverage partial rather than counting against the run.
    #[must_use]
    pub fn unavailable_provenance(&self) -> &[WantedQuote] {
        &self.unavailable_provenance
    }

    /// Overall coverage, or `None` while loading.
    #[must_use]
    pub fn coverage(&self) -> Option<Coverage> {
        match &self.state {
            TrustContextState::Loading => None,
            TrustContextState::Settled { coverage, .. } => Some(*coverage),
        }
    }

    /// Whether the canonical [`CONTEXT_INCOMPLETE_LABEL`] caveat applies.
    #[must_use]
    pub fn is_incomplete(&self) -> bool {
        self.coverage() != Some(Coverage::Complete)
    }

    /// The identity-level resolution for one signer.
    #[must_use]
    pub fn signer_resolution(&self, pubkey: &PublicKey) -> TrustResolution {
        trust_resolution(Some(&self.state), pubkey)
    }

    /// The resolution for one run, applying its frozen provenance.
    #[must_use]
    pub fn run_resolution(&self, run: &WorkflowRun) -> TrustResolution {
        run_trust_resolution(
            Some(&self.state),
            run,
            &self.confirmed_maintainers,
            &self.controls,
            &self.coordinates,
            &self.validated_provenance,
        )
    }

    /// The resolution for one Job Result within a run.
    #[must_use]
    pub fn job_resolution(&self, run: &WorkflowRun, job: &JobResult) -> TrustResolution {
        job_trust_resolution(Some(&self.state), run, job)
    }

    /// The conservative rollup across several runs.
    #[must_use]
    pub fn summarize(&self, runs: &[WorkflowRun]) -> TrustResolution {
        let resolutions: Vec<TrustResolution> =
            runs.iter().map(|run| self.run_resolution(run)).collect();
        summarize_run_trust(&resolutions)
    }

    /// The relationship tier for one coordinator.
    #[must_use]
    pub fn relationship(&self, pubkey: &PublicKey) -> CoordinatorRelationship {
        coordinator_relationship(&self.relationships, pubkey)
    }
}

/// Resolve from the local cache alone: no network, no NIP-05.
///
/// Coverage is partial whenever there is a signer to describe, because the
/// domain ladder was skipped for it.
#[must_use]
pub fn resolve_cache_tier(inputs: &CiInputs<'_>) -> CiTrustContext {
    let outcome = provenance::validate_run_quotes(
        inputs.runs,
        inputs.quoted_events,
        &inputs.repository.confirmed_maintainers,
        &inputs.repository.coordinates,
    );
    let controls = controls_in_closure(inputs);
    let coverage = if described_signers(inputs.runs, &controls).is_empty() {
        inputs.input_coverage
    } else {
        Coverage::Partial
    };
    assemble(inputs, controls, outcome, &HashMap::new(), coverage)
}

/// Resolve everything: fetch the missing quoted requests, then verify NIP-05
/// identities against the repository's GRASP domains.
///
/// A failed fetch or lookup settles as partial coverage with whatever
/// positive evidence was found; it never blocks and never produces a negative
/// claim.
pub async fn resolve_full_tier<F, L>(
    inputs: &CiInputs<'_>,
    fetcher: &F,
    lookup: &L,
    nip05_cache: Option<&Nip05Cache>,
) -> CiTrustContext
where
    F: QuotedEventFetcher + ?Sized + Sync,
    L: Nip05Lookup + ?Sized + Sync,
{
    let mut coverage = inputs.input_coverage;

    let missing: Vec<WantedQuote> = provenance::wanted_quotes(inputs.runs)
        .into_iter()
        .filter(|wanted| !inputs.quoted_events.contains_key(&wanted.event_id))
        .collect();
    let mut quoted_events = inputs.quoted_events.clone();
    if !missing.is_empty() {
        match fetcher.fetch(&missing).await {
            Ok(fetched) => quoted_events.extend(fetched.into_iter().map(|event| (event.id, event))),
            // The quotes stay unavailable below, which is what makes coverage
            // partial; the command still runs.
            Err(_) => coverage = Coverage::Partial,
        }
    }

    let outcome = provenance::validate_run_quotes(
        inputs.runs,
        &quoted_events,
        &inputs.repository.confirmed_maintainers,
        &inputs.repository.coordinates,
    );
    if !outcome.unavailable.is_empty() {
        coverage = Coverage::Partial;
    }

    let controls = controls_in_closure(inputs);
    let identities = verify_identities(
        lookup,
        nip05_cache,
        &identity_lookup_signers(
            inputs.runs,
            &controls,
            &inputs.repository.confirmed_maintainers,
        ),
        inputs.profile_nip05,
        &inputs.repository.grasp_domains,
        inputs.now,
    )
    .await;
    if any_lookup_failed(&identities) {
        coverage = Coverage::Partial;
    }

    assemble(inputs, controls, outcome, &identities, coverage)
}

/// The Service Requests and Stops that describe *this* repository.
///
/// Everything downstream — the relationship tiers, the signer set and the
/// per-run reduction — reads this filtered history, so a control naming a
/// coordinate outside the confirmed-member set cannot introduce a coordinator
/// or attribute a foreign repository's maintainers as its requesters.
fn controls_in_closure(inputs: &CiInputs<'_>) -> Vec<ServiceControl> {
    inputs
        .controls
        .iter()
        .filter(|control| {
            inputs
                .repository
                .coordinates
                .contains(&control.repository.coordinate)
        })
        .cloned()
        .collect()
}

/// Which controls contribute their coordinator to a signer set.
///
/// The one statement of the scoping rule from the NIP-05 bounding fix: the
/// display set describes every coordinator the control history addresses,
/// while the network-lookup set admits only controls a confirmed maintainer
/// authored.
enum ControlScope<'a> {
    /// Every control in the confirmed-member coordinate set.
    Any,
    /// Only controls one of these confirmed maintainers authored.
    MaintainerAuthored(&'a [PublicKey]),
}

/// The one signer-set constructor: the coordinator of every run described
/// and the provider of every job under it — the signers every surface shows
/// and the merge gate rolls up — then the coordinator of every control
/// `scope` admits. First-seen order, deduplicated.
fn collect_signers(
    runs: &[WorkflowRun],
    controls: &[ServiceControl],
    scope: &ControlScope,
) -> Vec<PublicKey> {
    let mut signers: Vec<PublicKey> = Vec::new();
    for run in runs {
        push_unique(&mut signers, run.coordinator);
        for job in &run.jobs {
            push_unique(&mut signers, job.author);
        }
    }
    for control in controls {
        let admitted = match scope {
            ControlScope::Any => true,
            ControlScope::MaintainerAuthored(maintainers) => maintainers.contains(&control.author),
        };
        if admitted {
            push_unique(&mut signers, control.coordinator);
        }
    }
    signers
}

fn push_unique(signers: &mut Vec<PublicKey>, signer: PublicKey) {
    if !signers.contains(&signer) {
        signers.push(signer);
    }
}

/// Every signer the view describes: run coordinators, job providers, and the
/// coordinators the repository's control history addresses.
fn described_signers(runs: &[WorkflowRun], controls: &[ServiceControl]) -> Vec<PublicKey> {
    collect_signers(runs, controls, &ControlScope::Any)
}

/// The signers whose NIP-05 identities are resolved over the network.
///
/// Deliberately narrower than [`described_signers`]: the run signers, which
/// every surface displays and the merge gate rolls up, plus the coordinators
/// of controls a *confirmed maintainer* authored
/// ([`ControlScope::MaintainerAuthored`]), which are the only controls that
/// yield a relationship tier. A control from anybody else names a
/// coordinator this repository has no relationship with and no surface
/// describes, so resolving it would buy nothing — while letting an unrelated
/// publisher add a domain of their choosing to the identity step of every
/// command that runs the full tier, including a default, non-blocking
/// `ngit pr merge`. Such a coordinator keeps its (empty) resolution and its
/// unassociated relationship; only the lookup is withheld.
#[must_use]
pub fn identity_lookup_signers(
    runs: &[WorkflowRun],
    controls: &[ServiceControl],
    confirmed_maintainers: &[PublicKey],
) -> Vec<PublicKey> {
    collect_signers(
        runs,
        controls,
        &ControlScope::MaintainerAuthored(confirmed_maintainers),
    )
}

/// Build the per-signer resolutions from the evidence each tier gathered.
fn assemble(
    inputs: &CiInputs<'_>,
    controls: Vec<ServiceControl>,
    outcome: provenance::ProvenanceOutcome,
    identities: &HashMap<PublicKey, Vec<VerifiedIdentity>>,
    coverage: Coverage,
) -> CiTrustContext {
    let repository = inputs.repository;
    let requested = requested_coordinators(
        &controls,
        &repository.coordinates,
        &repository.confirmed_maintainers,
        inputs.now,
    );
    let previously = previously_requested_coordinators(
        &controls,
        &repository.coordinates,
        &repository.confirmed_maintainers,
        inputs.now,
    );
    let relationships = classify_coordinator_relationships(
        inputs.runs,
        &repository.confirmed_maintainers,
        &requested,
        &previously,
        &controls,
        &outcome.validated,
    );

    let mut resolutions = HashMap::new();
    for signer in described_signers(inputs.runs, &controls)
        .into_iter()
        .chain(relationships.keys().copied())
    {
        if resolutions.contains_key(&signer) {
            continue;
        }
        let mut evidence = relationship_evidence(relationships.get(&signer));
        evidence.extend(domain_evidence(
            identities.get(&signer).map_or(&[][..], Vec::as_slice),
            &repository.grasp_domains,
        ));
        // Level 3 seen-in-your-network evidence would be appended here.
        resolutions.insert(signer, settled_trust_resolution(evidence, coverage));
    }

    CiTrustContext {
        state: TrustContextState::Settled {
            resolutions,
            coverage,
        },
        relationships,
        validated_provenance: outcome.validated,
        rejected_provenance: outcome.rejected,
        unavailable_provenance: outcome.unavailable,
        confirmed_maintainers: repository.confirmed_maintainers.clone(),
        coordinates: repository.coordinates.clone(),
        controls,
    }
}

/// The canonical caveat, re-exported so callers do not reach past this module
/// for it.
#[must_use]
pub fn context_incomplete_label() -> &'static str {
    CONTEXT_INCOMPLETE_LABEL
}

/// Whether a settled resolution carries no evidence at all.
#[must_use]
pub fn has_no_known_context(resolution: &TrustResolution) -> bool {
    resolution.classification() == Some(TrustClassification::NoKnownContext)
}

#[cfg(test)]
mod tests {
    use nostr::prelude::Keys;

    use super::{
        super::{
            controls::CoordinatorRelationshipLevel,
            domain::tests::{CountingLookup, StubNip05Lookup},
            kinds::test_events::*,
            provenance::tests::{
                manual_trigger_run, perspective, run_on_another_commit, service_request_run,
            },
            trust::{EvidenceScope, TrustEvidenceKind},
        },
        *,
    };

    fn repository(owner: &Keys, maintainer: &Keys) -> RepositoryContext {
        RepositoryContext {
            confirmed_maintainers: vec![maintainer.public_key()],
            coordinates: vec![perspective(owner)],
            grasp_domains: vec!["grasp.example".to_owned()],
        }
    }

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_secs(secs)
    }

    /// A fetcher that returns the given events, or fails.
    struct StubFetcher {
        events: Vec<Event>,
        fails: bool,
    }

    #[async_trait]
    impl QuotedEventFetcher for StubFetcher {
        async fn fetch(&self, _wanted: &[WantedQuote]) -> Result<Vec<Event>> {
            if self.fails {
                anyhow::bail!("relays unreachable");
            }
            Ok(self.events.clone())
        }
    }

    #[test]
    fn the_cache_tier_settles_partial_because_domain_checks_were_skipped() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);
        let repository = repository(&owner, &maintainer);
        let quoted: HashMap<EventId, Event> = [(request.id, request.clone())].into_iter().collect();
        let profiles = HashMap::new();
        let runs = [run];

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &runs,
            &[],
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert_eq!(context.coverage(), Some(Coverage::Partial));
        assert!(context.is_incomplete());
        // The locally cached quote is still validated, so the run keeps its
        // maintainer direction.
        assert_eq!(context.validated_provenance().len(), 1);
        assert!(context.validated_provenance()[0].covers(&runs[0]));
        assert_eq!(context.validated_provenance()[0].quote(), request.id);
        assert_eq!(
            context.run_resolution(&runs[0]).classification(),
            Some(TrustClassification::MaintainerDirected)
        );
    }

    #[test]
    fn a_view_with_no_signers_needs_no_caveat() {
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let repository = repository(&owner, &maintainer);
        let quoted = HashMap::new();
        let profiles = HashMap::new();

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &[],
            &[],
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert_eq!(context.coverage(), Some(Coverage::Complete));
    }

    #[test]
    fn caller_side_query_failures_make_the_context_partial() {
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let repository = repository(&owner, &maintainer);
        let quoted = HashMap::new();
        let profiles = HashMap::new();
        let mut inputs = CiInputs::new(&repository, &[], &[], &quoted, &profiles, ts(1_000));
        inputs.input_coverage = Coverage::Partial;

        assert_eq!(
            resolve_cache_tier(&inputs).coverage(),
            Some(Coverage::Partial)
        );
    }

    #[test]
    fn an_unavailable_quote_yields_no_maintainer_direction_in_the_cache_tier() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);
        let repository = repository(&owner, &maintainer);
        let quoted = HashMap::new();
        let profiles = HashMap::new();
        let runs = [run];

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &runs,
            &[],
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert!(context.validated_provenance().is_empty());
        assert_eq!(
            context.unavailable_provenance()[0].event_id,
            request.id,
            "the quote is reported as unchecked, not rejected"
        );
        assert!(context.rejected_provenance().is_empty());
        assert_eq!(
            context.run_resolution(&runs[0]).classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert!(context.is_incomplete());
    }

    #[tokio::test]
    async fn the_full_tier_settles_complete_with_verified_identities() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);
        let repository = repository(&owner, &maintainer);
        let quoted: HashMap<EventId, Event> = [(request.id, request.clone())].into_iter().collect();
        let profiles = HashMap::new();
        let runs = [run];

        let context = resolve_full_tier(
            &CiInputs::new(&repository, &runs, &[], &quoted, &profiles, ts(1_000)),
            &NoQuotedEventFetcher,
            &StubNip05Lookup::resolving("_@grasp.example", coordinator.public_key()),
            None,
        )
        .await;

        assert_eq!(context.coverage(), Some(Coverage::Complete));
        assert!(!context.is_incomplete());
        let resolution = context.signer_resolution(&coordinator.public_key());
        let kinds: Vec<TrustEvidenceKind> =
            resolution.evidence().iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            vec![
                // The validated quote is maintainer direction for the run, and
                // earlier direction for the coordinator identity.
                TrustEvidenceKind::HistoricalMaintainerRequest,
                TrustEvidenceKind::RepositoryDomain,
            ]
        );
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::OperationallyAssociated)
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_keeps_positive_evidence_and_settles_partial() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let repository = RepositoryContext {
            confirmed_maintainers: vec![maintainer.public_key()],
            coordinates: vec![perspective(&owner)],
            grasp_domains: vec!["grasp.example".to_owned(), "mirror.example".to_owned()],
        };
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);
        let quoted: HashMap<EventId, Event> = [(request.id, request.clone())].into_iter().collect();
        let profiles = HashMap::new();
        let runs = [run];

        // `grasp.example` resolves; `mirror.example` does not.
        let context = resolve_full_tier(
            &CiInputs::new(&repository, &runs, &[], &quoted, &profiles, ts(1_000)),
            &NoQuotedEventFetcher,
            &StubNip05Lookup::resolving("_@grasp.example", coordinator.public_key()),
            None,
        )
        .await;

        assert_eq!(context.coverage(), Some(Coverage::Partial));
        let resolution = context.signer_resolution(&coordinator.public_key());
        assert_eq!(
            resolution.classification(),
            Some(TrustClassification::OperationallyAssociated),
            "the verified domain is still described"
        );
        assert!(
            resolution
                .evidence()
                .iter()
                .any(|item| item.kind == TrustEvidenceKind::RepositoryDomain),
            "the domain that did resolve keeps its evidence"
        );
        assert!(
            !resolution
                .evidence()
                .iter()
                .any(|item| item.kind == TrustEvidenceKind::RepositorySubdomain),
            "the domain that did not resolve makes no claim either way"
        );
    }

    #[tokio::test]
    async fn a_fetched_quote_flows_into_maintainer_directed_run_resolution() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, run) = service_request_run(&coordinator, &owner, &maintainer);
        let repository = repository(&owner, &maintainer);
        let quoted = HashMap::new();
        let profiles = HashMap::new();
        let runs = [run];
        let inputs = CiInputs::new(&repository, &runs, &[], &quoted, &profiles, ts(1_000));

        let fetched = resolve_full_tier(
            &inputs,
            &StubFetcher {
                events: vec![request.clone()],
                fails: false,
            },
            &StubNip05Lookup::default(),
            None,
        )
        .await;
        assert_eq!(fetched.validated_provenance().len(), 1);
        assert!(fetched.validated_provenance()[0].covers(&runs[0]));
        assert_eq!(fetched.validated_provenance()[0].quote(), request.id);
        assert_eq!(
            fetched.run_resolution(&runs[0]).classification(),
            Some(TrustClassification::MaintainerDirected)
        );
        assert_eq!(
            fetched.run_resolution(&runs[0]).evidence()[0].scope,
            EvidenceScope::Run
        );

        // A fetch that fails leaves the quote unchecked: no evidence, partial
        // coverage, and no negative claim.
        let failed = resolve_full_tier(
            &inputs,
            &StubFetcher {
                events: Vec::new(),
                fails: true,
            },
            &StubNip05Lookup::default(),
            None,
        )
        .await;
        assert!(failed.validated_provenance().is_empty());
        assert_eq!(
            failed.run_resolution(&runs[0]).classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert_eq!(failed.coverage(), Some(Coverage::Partial));
    }

    #[tokio::test]
    async fn a_quote_that_fails_validation_is_reported_and_carries_no_evidence() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let stranger = Keys::generate();
        let maintainer = Keys::generate();
        // The run quotes a request signed by somebody who is not a maintainer.
        let (request, run) = service_request_run(&coordinator, &owner, &stranger);
        let repository = repository(&owner, &maintainer);
        let quoted: HashMap<EventId, Event> = [(request.id, request.clone())].into_iter().collect();
        let profiles = HashMap::new();
        let runs = [run];

        let context = resolve_full_tier(
            &CiInputs::new(&repository, &runs, &[], &quoted, &profiles, ts(1_000)),
            &NoQuotedEventFetcher,
            &StubNip05Lookup::default(),
            None,
        )
        .await;
        assert!(context.validated_provenance().is_empty());
        assert_eq!(context.rejected_provenance().len(), 1);
        assert_eq!(
            context.run_resolution(&runs[0]).classification(),
            Some(TrustClassification::NoKnownContext)
        );
    }

    #[test]
    fn a_trigger_validated_for_one_run_does_not_legitimize_another() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        // Both runs quote the same Manual Trigger; the second ran a commit
        // the maintainer never authorized.
        let (trigger, authorized) = manual_trigger_run(&coordinator, &owner, &maintainer);
        let replayed =
            run_on_another_commit(&coordinator, &owner, &maintainer, "run-2", trigger.id);
        let repository = repository(&owner, &maintainer);
        let quoted: HashMap<EventId, Event> = [(trigger.id, trigger.clone())].into_iter().collect();
        let profiles = HashMap::new();
        let runs = [authorized, replayed];

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &runs,
            &[],
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert_eq!(context.validated_provenance().len(), 1);
        assert!(context.validated_provenance()[0].covers(&runs[0]));
        assert_eq!(context.validated_provenance()[0].quote(), trigger.id);
        assert_eq!(
            context.run_resolution(&runs[0]).classification(),
            Some(TrustClassification::MaintainerDirected)
        );
        assert_eq!(
            context.run_resolution(&runs[1]).classification(),
            // The coordinator identity keeps the earlier direction the
            // authorized run established, but this run gains none of its own.
            Some(TrustClassification::OperationallyAssociated),
            "the trigger authorized the other run, so this one is not directed"
        );
        assert_eq!(context.rejected_provenance().len(), 1);
        // The gate reads the rollup, which the unauthorized run must weaken
        // below the maintainer-directed floor.
        assert_eq!(
            context.summarize(&runs).classification(),
            Some(TrustClassification::OperationallyAssociated)
        );
    }

    #[test]
    fn control_history_still_establishes_the_relationship_tier() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let repository = repository(&owner, &maintainer);
        let controls = vec![super::super::controls::tests::control(
            &maintainer,
            &coordinator,
            &owner,
            true,
            100,
            1,
        )];
        let quoted = HashMap::new();
        let profiles = HashMap::new();

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &[],
            &controls,
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert_eq!(
            context.relationship(&coordinator.public_key()).level,
            CoordinatorRelationshipLevel::Requested
        );
        assert_eq!(
            context
                .signer_resolution(&coordinator.public_key())
                .classification(),
            Some(TrustClassification::MaintainerDirected)
        );
    }

    #[test]
    fn a_request_on_a_foreign_coordinate_introduces_nothing() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let other_owner = Keys::generate();
        let maintainer = Keys::generate();
        let repository = repository(&owner, &maintainer);
        // A maintainer of this repository asks a coordinator to serve a
        // *different* repository. That says nothing here.
        let controls = vec![super::super::controls::tests::control(
            &maintainer,
            &coordinator,
            &other_owner,
            true,
            100,
            1,
        )];
        let quoted = HashMap::new();
        let profiles = HashMap::new();

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &[],
            &controls,
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert!(
            context.relationships.is_empty(),
            "no relationship, so no requester attribution from a foreign repository"
        );
        let TrustContextState::Settled { resolutions, .. } = &context.state else {
            panic!("the cache tier settles");
        };
        assert!(
            resolutions.is_empty(),
            "the coordinator is not a signer of this view"
        );
        assert_eq!(
            context.relationship(&coordinator.public_key()).level,
            CoordinatorRelationshipLevel::Unassociated
        );
        assert_eq!(context.coverage(), Some(Coverage::Complete));
    }

    #[test]
    fn only_a_maintainers_control_adds_a_coordinator_to_the_identity_lookup_set() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let stranger = Keys::generate();
        let requested = Keys::generate();
        let unrelated = Keys::generate();

        let grouped = super::super::events::group_workflow_runs(&[
            workflow_result(&coordinator, &owner, "run-1", 100),
            job_result(&provider, &coordinator, &owner, "run-1", "build", 110),
        ]);
        assert!(grouped.skipped.is_empty(), "{:?}", grouped.skipped);
        let controls = vec![
            super::super::controls::tests::control(&maintainer, &requested, &owner, true, 100, 1),
            // Anyone may sign a Service Request naming this repository's
            // coordinate and any coordinator pubkey.
            super::super::controls::tests::control(&stranger, &unrelated, &owner, true, 100, 2),
        ];

        let lookup_set =
            identity_lookup_signers(&grouped.runs, &controls, &[maintainer.public_key()]);
        assert!(
            lookup_set.contains(&coordinator.public_key()),
            "the run's coordinator is displayed and read by the gate"
        );
        assert!(
            lookup_set.contains(&provider.public_key()),
            "so is the provider of a job under it"
        );
        assert!(
            lookup_set.contains(&requested.public_key()),
            "a confirmed maintainer's request survives authorization"
        );
        assert!(
            !lookup_set.contains(&unrelated.public_key()),
            "a coordinator known only through an unauthorized control is not resolved"
        );
        // It is still a signer of the view — it simply gets no lookup.
        assert!(described_signers(&grouped.runs, &controls).contains(&unrelated.public_key()));
    }

    #[tokio::test]
    async fn the_full_tier_looks_up_no_identity_for_an_unauthorized_control() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let stranger = Keys::generate();
        let unrelated = Keys::generate();
        let repository = repository(&owner, &maintainer);
        let runs = super::super::events::group_workflow_runs(&[workflow_result(
            &coordinator,
            &owner,
            "run-1",
            100,
        )])
        .runs;
        let controls = vec![super::super::controls::tests::control(
            &stranger, &unrelated, &owner, true, 100, 1,
        )];
        let profiles: HashMap<PublicKey, String> = [
            (coordinator.public_key(), "act-1@grasp.example".to_owned()),
            (unrelated.public_key(), "slow@attacker.example".to_owned()),
        ]
        .into_iter()
        .collect();
        let quoted = HashMap::new();

        let lookup = CountingLookup::resolving("act-1@grasp.example", coordinator.public_key());
        let context = resolve_full_tier(
            &CiInputs::new(&repository, &runs, &controls, &quoted, &profiles, ts(1_000)),
            &NoQuotedEventFetcher,
            &lookup,
            None,
        )
        .await;

        let looked_up = lookup.looked_up();
        assert!(
            !looked_up
                .iter()
                .any(|address| address.contains("attacker.example")),
            "an unrelated publisher's domain never reaches the identity step: {looked_up:?}"
        );
        assert_eq!(
            looked_up.len(),
            2,
            "only the run coordinator's declared identity and the repository root: {looked_up:?}"
        );
        assert_eq!(
            context
                .signer_resolution(&coordinator.public_key())
                .classification(),
            Some(TrustClassification::OperationallyAssociated),
            "the signers that are resolved keep their domain evidence"
        );
        assert_eq!(
            context
                .signer_resolution(&unrelated.public_key())
                .classification(),
            Some(TrustClassification::NoKnownContext),
        );
    }

    #[test]
    fn a_loading_context_never_reports_no_known_context() {
        let coordinator = Keys::generate();
        let owner = Keys::generate();
        let grouped = super::super::events::group_workflow_runs(&[workflow_result(
            &coordinator,
            &owner,
            "run-1",
            100,
        )]);
        let run = &grouped.runs[0];
        let context = CiTrustContext::loading();

        assert_eq!(
            context.signer_resolution(&coordinator.public_key()),
            TrustResolution::Loading
        );
        assert_eq!(context.run_resolution(run), TrustResolution::Loading);
        assert_eq!(context.summarize(&grouped.runs), TrustResolution::Loading);
        assert!(!has_no_known_context(&context.run_resolution(run)));
        assert_eq!(context.coverage(), None);
        assert!(context.is_incomplete());
    }

    #[test]
    fn the_rollup_reports_the_weakest_run() {
        let coordinator = Keys::generate();
        let other = Keys::generate();
        let owner = Keys::generate();
        let maintainer = Keys::generate();
        let (request, requested_run) = service_request_run(&coordinator, &owner, &maintainer);
        let unrequested = super::super::events::group_workflow_runs(&[workflow_result(
            &other, &owner, "run-2", 100,
        )])
        .runs
        .remove(0);
        let repository = repository(&owner, &maintainer);
        let quoted: HashMap<EventId, Event> = [(request.id, request)].into_iter().collect();
        let profiles = HashMap::new();
        let runs = [requested_run, unrequested];

        let context = resolve_cache_tier(&CiInputs::new(
            &repository,
            &runs,
            &[],
            &quoted,
            &profiles,
            ts(1_000),
        ));
        assert_eq!(
            context.summarize(&runs).classification(),
            Some(TrustClassification::NoKnownContext)
        );
        assert_eq!(context.summarize(&runs).coverage(), Some(Coverage::Partial));
        assert_eq!(context_incomplete_label(), "Context incomplete");
    }
}
