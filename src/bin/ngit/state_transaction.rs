//! The repository-state publication transaction.
//!
//! A normal `git push` to the nostr remote publishes an updated repository
//! state event alongside the git data it references. GRASP relays that
//! support purgatory hold repo state events until the matching git data
//! arrives at the paired git server. Publishing to them before the git
//! push is therefore safe: if the later git push fails, the relay never
//! broadcasts the unreachable state.
//!
//! This intentionally makes state publication a staged process. First
//! seed purgatory on the GRASP relays we are about to push to, then push
//! git data, and only after at least one git server accepts the data fan
//! out the state to any remaining relays. The extra round-trip prevents
//! us reporting `ok` or broadcasting state for commits that no git
//! server has.
//!
//! [`StateTransaction`] owns those phases plus the commit point: the
//! candidate state event is published without touching the local event
//! cache ([`StateTransactionOps::publish_state_events`]) and becomes
//! locally authoritative only via [`StateTransaction::commit`], after a
//! git server accepted the pushed data and at least one relay accepted
//! the event. Failure at any earlier point leaves the
//! previously cached state untouched — there is nothing to roll back.
//! The caller keeps ownership of user-facing reporting;
//! [`StateTransactionFailure`] keeps the failure reasons typed in
//! between.
//!
//! `ngit sync` and `ngit init` still run their own divergent copies of
//! this flow; migrating them onto this module is planned follow-up work.

use std::collections::HashMap;

use anyhow::{Context, Result};
use console::Term;
use ngit::{
    client::{Client, save_event_in_local_cache, send_events, send_events_without_caching},
    git::{Repo, RepoActions, nostr_url::NostrUrlDecoded},
    push::push_to_remote,
    repo_ref::{
        RepoRef, format_grasp_server_url_as_relay_url, grasp_server_relay_urls,
        is_grasp_server_clone_url,
    },
    repo_state::RepoState,
    utils::get_short_git_server_name,
};
use nostr::{Event, RelayUrl};

/// Side-effecting operations used by [`StateTransaction`]. Injected so the
/// transaction phases can be exercised deterministically in tests; the
/// production implementation is [`LiveOps`].
pub trait StateTransactionOps {
    /// Publish `events` to the given relays and report per-relay
    /// acceptance. Successfully sent events are also written into the
    /// local event cache.
    async fn publish_events(
        &mut self,
        events: Vec<Event>,
        my_write_relays: Vec<String>,
        repo_relays: Vec<RelayUrl>,
    ) -> Result<Vec<(String, bool)>>;

    /// Publish candidate state `events` to the given relays and report
    /// per-relay acceptance, guaranteeing no write to the local event
    /// cache: an unverified candidate must not become locally
    /// authoritative as a publication side effect.
    async fn publish_state_events(
        &mut self,
        events: Vec<Event>,
        my_write_relays: Vec<String>,
        repo_relays: Vec<RelayUrl>,
    ) -> Result<Vec<(String, bool)>>;

    /// Push `refspecs` to `git_server_url`, returning per-ref rejection
    /// reasons (`None` meaning the ref update was accepted).
    fn push_to_git_server(
        &mut self,
        git_server_url: &str,
        refspecs: &[String],
    ) -> Result<HashMap<String, Option<String>>>;

    /// Store `event` in the local nostr event cache.
    async fn save_event_in_cache(&mut self, event: &Event) -> Result<()>;
}

/// [`StateTransactionOps`] implementation backed by the nostr client, the
/// local git repository's event cache and the real git servers.
pub struct LiveOps<'a> {
    pub client: &'a Client,
    pub git_repo: &'a Repo,
    pub term: &'a Term,
    pub git_server_push_options: &'a [String],
    /// The decoded `nostr://` URL the git pushes run under. Its protocol
    /// and ssh-key overrides must come from the caller because they are
    /// remote-specific: `ngit sync` pushes under the user's configured
    /// remote URL, which can differ from the repo-ref derived default.
    pub decoded_nostr_url: &'a NostrUrlDecoded,
}

impl StateTransactionOps for LiveOps<'_> {
    async fn publish_events(
        &mut self,
        events: Vec<Event>,
        my_write_relays: Vec<String>,
        repo_relays: Vec<RelayUrl>,
    ) -> Result<Vec<(String, bool)>> {
        send_events(
            self.client,
            Some(self.git_repo.get_path()?),
            events,
            my_write_relays,
            repo_relays,
            true,
            false,
        )
        .await
    }

    async fn publish_state_events(
        &mut self,
        events: Vec<Event>,
        my_write_relays: Vec<String>,
        repo_relays: Vec<RelayUrl>,
    ) -> Result<Vec<(String, bool)>> {
        send_events_without_caching(
            self.client,
            Some(self.git_repo.get_path()?),
            events,
            my_write_relays,
            repo_relays,
            true,
            false,
        )
        .await
    }

    fn push_to_git_server(
        &mut self,
        git_server_url: &str,
        refspecs: &[String],
    ) -> Result<HashMap<String, Option<String>>> {
        let push_options_refs: Vec<&str> = self
            .git_server_push_options
            .iter()
            .map(String::as_str)
            .collect();
        push_to_remote(
            self.git_repo,
            git_server_url,
            self.decoded_nostr_url,
            refspecs,
            self.term,
            is_grasp_server_clone_url(git_server_url),
            &push_options_refs,
        )
    }

    async fn save_event_in_cache(&mut self, event: &Event) -> Result<()> {
        save_event_in_local_cache(self.git_repo.get_path()?, event)
            .await
            .map(|_| ())
    }
}

/// Which git servers a push may realign destructively.
///
/// The plan builder marks a refspec that cannot fast-forward with a
/// leading `+` and encodes a ref deletion as an empty push source
/// (`:refs/...`). The policy decides per server whether such destructive
/// refspecs are executed or dropped from that server's plan; dropped
/// refspecs are recorded on the transaction for the caller's reporting.
pub enum ServerForcePolicy {
    /// Execute destructive refspecs on every server. This is the default
    /// and the remote helper's behaviour: any out-of-sync server is
    /// realigned to the pushed state.
    ForceRealignAll,
    /// Execute destructive refspecs only on the listed git servers
    /// (compared ignoring a trailing slash); every other server is pushed
    /// fast-forward-only. `ngit sync` realigns GRASP servers but leaves
    /// vanilla servers untouched unless `--force`.
    ForceOnlyOn(Vec<String>),
}

impl ServerForcePolicy {
    fn allows_force(&self, git_server_url: &str) -> bool {
        match self {
            Self::ForceRealignAll => true,
            Self::ForceOnlyOn(git_servers) => git_servers
                .iter()
                .any(|url| url.trim_end_matches('/') == git_server_url.trim_end_matches('/')),
        }
    }
}

/// Whether pushing `refspec` can discard existing server-side work: a
/// forced update (`+` prefix) or a ref deletion (empty push source).
fn is_destructive_refspec(refspec: &str) -> bool {
    refspec.starts_with('+') || refspec.starts_with(':')
}

/// Destructive refspecs dropped from per-server plans by the
/// [`ServerForcePolicy`], keyed by git server URL.
type DroppedRefspecs = HashMap<String, Vec<String>>;

/// Outcome of executing the per-server git push plans.
pub enum GitStatePushOutcome {
    /// No git server remained eligible for a push (e.g. every paired GRASP
    /// relay rejected the staged state event).
    NoEligibleServers,
    /// Every eligible git server with pending changes rejected the git
    /// data push.
    AllPushesFailed,
    /// At least one git server accepted every ref update pushed to it or
    /// already had every requested change applied (an empty per-server
    /// plan).
    AcceptedByGitServer,
}

/// Per-git-server outcome of the git data push phase, recorded during
/// [`StateTransaction::push_git_state_refspecs`] for the caller's
/// reporting. Servers the GRASP staging gate skipped are absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerPushOutcome {
    /// The server's plan was empty — every requested change is already
    /// applied there (possibly because the [`ServerForcePolicy`] dropped
    /// the plan's destructive refspecs) — so it counted as a successful
    /// push target without being contacted.
    AlreadyApplied,
    /// The server accepted every ref update pushed to it.
    Accepted,
    /// The push completed but the server rejected at least one ref
    /// update.
    RefsRejected,
    /// The push failed outright (e.g. connection or protocol error).
    Failed,
}

/// Why the transaction failed to establish the replacement state.
pub enum StateTransactionFailure {
    /// See [`GitStatePushOutcome::NoEligibleServers`].
    NoEligibleGitServers,
    /// See [`GitStatePushOutcome::AllPushesFailed`].
    AllGitServerPushesFailed,
    /// Git data was pushed but no relay accepted the state event.
    StateNotAcceptedByAnyRelay,
}

impl StateTransactionFailure {
    /// Reason text the remote helper reports against each failed ref.
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::NoEligibleGitServers => "state event failed to reach any git server relay",
            Self::AllGitServerPushesFailed => "failed to push to any git server",
            Self::StateNotAcceptedByAnyRelay => "state event failed to reach any relay",
        }
    }
}

#[derive(Default)]
struct InitialStatePublish {
    grasp_relays: Vec<RelayUrl>,
    results: Vec<(String, bool)>,
}

/// How the transaction relates to the state event it publishes.
enum StateMode {
    /// A candidate replacement state built by the caller; it becomes
    /// locally authoritative only at [`StateTransaction::commit`].
    Candidate,
    /// An already-canonical state event (e.g. loaded from the local
    /// cache): no cache write is needed and staging only seeds the
    /// GRASP relays that are missing the event.
    Authoritative {
        /// relays known to already hold the current state event
        relays_already_holding: Vec<RelayUrl>,
    },
}

/// A single repository-state publication transaction (see the module docs
/// for the phase ordering rationale).
///
/// The caller builds and signs the candidate state event — ordered after
/// the previous cached event under NIP-01 — but must not cache it: the
/// previous event remains the authoritative ordering reference until
/// [`Self::commit`] performs the transaction's single cache write after
/// git server and relay acceptance.
///
/// [`Self::new_authoritative`] instead propagates an existing canonical
/// state event: staging seeds only the GRASP relays missing it and the
/// transaction performs no cache write at all.
pub struct StateTransaction<'a> {
    repo_ref: &'a RepoRef,
    /// the candidate replacement state (`None` when the push publishes no
    /// state event, e.g. `nostr.nostate`)
    state: Option<RepoState>,
    mode: StateMode,
    force_policy: ServerForcePolicy,
    initial_state_publish: InitialStatePublish,
    remaining_relay_results: Vec<(String, bool)>,
    refspecs_dropped_by_policy: HashMap<String, Vec<String>>,
    server_push_outcomes: HashMap<String, ServerPushOutcome>,
}

impl<'a> StateTransaction<'a> {
    pub fn new(repo_ref: &'a RepoRef, state: Option<RepoState>) -> Self {
        Self {
            repo_ref,
            state,
            mode: StateMode::Candidate,
            force_policy: ServerForcePolicy::ForceRealignAll,
            initial_state_publish: InitialStatePublish::default(),
            remaining_relay_results: vec![],
            refspecs_dropped_by_policy: HashMap::new(),
            server_push_outcomes: HashMap::new(),
        }
    }

    /// A transaction that propagates an *already-canonical* state event
    /// rather than a candidate: GRASP relays not listed in
    /// `relays_already_holding` are seeded with the existing event
    /// before their paired servers are pushed, the remaining-relay
    /// fanout is skipped, relay acceptance is vacuously satisfied and
    /// [`Self::commit`] performs no cache write.
    pub fn new_authoritative(
        repo_ref: &'a RepoRef,
        state: RepoState,
        relays_already_holding: Vec<RelayUrl>,
    ) -> Self {
        Self {
            repo_ref,
            state: Some(state),
            mode: StateMode::Authoritative {
                relays_already_holding,
            },
            force_policy: ServerForcePolicy::ForceRealignAll,
            initial_state_publish: InitialStatePublish::default(),
            remaining_relay_results: vec![],
            refspecs_dropped_by_policy: HashMap::new(),
            server_push_outcomes: HashMap::new(),
        }
    }

    /// Replace the default [`ServerForcePolicy::ForceRealignAll`] policy.
    pub fn with_force_policy(mut self, force_policy: ServerForcePolicy) -> Self {
        self.force_policy = force_policy;
        self
    }

    /// Destructive refspecs dropped per git server by the
    /// [`ServerForcePolicy`] during [`Self::push_git_state_refspecs`],
    /// for the caller's reporting.
    pub fn refspecs_dropped_by_policy(&self) -> &DroppedRefspecs {
        &self.refspecs_dropped_by_policy
    }

    /// Per-git-server outcomes recorded by
    /// [`Self::push_git_state_refspecs`], for the caller's reporting.
    /// Servers skipped by the GRASP staging gate are absent.
    pub fn server_push_outcomes(&self) -> &HashMap<String, ServerPushOutcome> {
        &self.server_push_outcomes
    }

    /// The candidate state events to publish (currently at most one).
    fn state_events(&self) -> Vec<Event> {
        self.state
            .as_ref()
            .map(|state| vec![state.event.clone()])
            .unwrap_or_default()
    }

    /// Publish state events to GRASP relays before pushing git data.
    ///
    /// This relies on GRASP purgatory: the relay accepts the event but
    /// withholds broadcast until the paired git server receives the
    /// referenced objects. That lets us target GRASP servers first without
    /// leaking a state event for data that later fails to push.
    ///
    /// A staging failure is not fatal to the transaction: it is recorded
    /// as "no relay accepted" (making every paired GRASP server
    /// ineligible for the git push) while vanilla git servers and the
    /// later relay fanout still get their chance.
    pub async fn publish_state_to_grasps_first(&mut self, ops: &mut impl StateTransactionOps) {
        let state_events = self.state_events();
        let grasp_relays = if state_events.is_empty() {
            vec![]
        } else {
            grasp_server_relay_urls(&self.repo_ref.git_server)
        };

        // In authoritative mode the event is already canonical: only
        // relays missing it are seeded, and the relays that hold it
        // count as having accepted it without a publish.
        let (relays_to_publish, relays_holding) = match &self.mode {
            StateMode::Candidate => (grasp_relays.clone(), vec![]),
            StateMode::Authoritative {
                relays_already_holding,
            } => grasp_relays.iter().cloned().partition(|relay| {
                !relays_already_holding
                    .iter()
                    .any(|holding| relay_urls_match(holding.as_str(), relay.as_str()))
            }),
        };

        let mut results = if state_events.is_empty() || relays_to_publish.is_empty() {
            vec![]
        } else {
            match ops
                .publish_state_events(state_events, vec![], relays_to_publish)
                .await
            {
                Ok(results) => results,
                Err(error) => {
                    eprintln!("WARNING: failed to stage state event on grasp relays: {error:#}");
                    vec![]
                }
            }
        };
        results.extend(
            relays_holding
                .into_iter()
                .map(|relay| (relay.to_string(), true)),
        );

        self.initial_state_publish = InitialStatePublish {
            grasp_relays,
            results,
        };
    }

    /// Execute the per-server git push plans for the servers still
    /// eligible after GRASP staging.
    ///
    /// A server whose plan is empty (every requested change is already
    /// applied there, e.g. deleting an already-absent branch or a ref
    /// already at the desired object) is not pushed to but counts as a
    /// successful push target — including when the [`ServerForcePolicy`]
    /// dropped the whole plan. Beyond that, a server's report-status
    /// acknowledgment is trusted as confirmation that the refs are
    /// served, matching git's own trust model; a GRASP server
    /// additionally guarantees its refs and promoted events are
    /// queryable before its push response completes.
    pub fn push_git_state_refspecs(
        &mut self,
        ops: &mut impl StateTransactionOps,
        remote_refspecs: HashMap<String, Vec<String>>,
        git_state_refspecs: &[String],
    ) -> GitStatePushOutcome {
        if git_state_refspecs.is_empty() {
            return GitStatePushOutcome::AcceptedByGitServer;
        }

        let (servers_to_push, refspecs_dropped_by_policy) = eligible_git_servers(
            remote_refspecs,
            git_state_refspecs,
            self.state.is_some(),
            &self.initial_state_publish.results,
            &self.force_policy,
        );
        self.refspecs_dropped_by_policy = refspecs_dropped_by_policy;

        if servers_to_push.is_empty() {
            return GitStatePushOutcome::NoEligibleServers;
        }

        let mut any_server_succeeded = false;
        for (git_server_url, server_refspecs) in &servers_to_push {
            if server_refspecs.is_empty() {
                // Every requested change is already applied on this
                // server, so it counts as success without a push.
                any_server_succeeded = true;
                self.server_push_outcomes
                    .insert(git_server_url.clone(), ServerPushOutcome::AlreadyApplied);
                continue;
            }
            let outcome = match ops.push_to_git_server(git_server_url, server_refspecs) {
                Ok(ref_updates) if all_ref_updates_accepted(&ref_updates) => {
                    any_server_succeeded = true;
                    ServerPushOutcome::Accepted
                }
                Ok(_) => ServerPushOutcome::RefsRejected,
                Err(_) => ServerPushOutcome::Failed,
            };
            self.server_push_outcomes
                .insert(git_server_url.clone(), outcome);
        }

        if any_server_succeeded {
            GitStatePushOutcome::AcceptedByGitServer
        } else {
            GitStatePushOutcome::AllPushesFailed
        }
    }

    /// Fan the state events out to the repo/user relays that were not
    /// already targeted during GRASP staging. A no-op in authoritative
    /// mode: an already-canonical event is only seeded where it is
    /// missing.
    pub async fn publish_state_to_remaining_relays(
        &mut self,
        ops: &mut impl StateTransactionOps,
        my_write_relays: &[String],
        repo_relay_only: bool,
    ) -> Result<()> {
        if matches!(self.mode, StateMode::Authoritative { .. }) {
            return Ok(());
        }
        let state_events = self.state_events();
        if state_events.is_empty() {
            return Ok(());
        }

        let excluded_relays = Some(self.initial_state_publish.grasp_relays.as_slice());
        let (repo_relays, write_relays) = relay_publish_targets(
            &self.repo_ref.relays,
            my_write_relays,
            repo_relay_only,
            excluded_relays,
        );
        if should_skip_empty_relay_publish(&repo_relays, &write_relays, excluded_relays) {
            return Ok(());
        }

        self.remaining_relay_results = ops
            .publish_state_events(state_events, write_relays, repo_relays)
            .await?;
        Ok(())
    }

    /// Whether at least one relay accepted the state event across the
    /// initial GRASP staging and the remaining-relay fanout (vacuously
    /// true when the transaction carries no state event, or in
    /// authoritative mode, where the event is already canonical and
    /// propagation is not gated on further relay acceptance).
    pub fn state_relay_accepted(&self) -> bool {
        if matches!(self.mode, StateMode::Authoritative { .. }) {
            return true;
        }
        state_relay_accepted(
            self.state.is_some(),
            self.initial_state_publish.results.iter(),
            self.remaining_relay_results.iter(),
        )
    }

    /// Persist the accepted candidate state event as the authoritative
    /// local state. This is the transaction's only cache write; every
    /// failure path before it leaves the previously cached state
    /// untouched, so a subsequent push or `ngit sync` keeps working from
    /// the last state a git server accepted. A no-op in authoritative
    /// mode: the event is already the cached authoritative state.
    pub async fn commit(&self, ops: &mut impl StateTransactionOps) -> Result<()> {
        if matches!(self.mode, StateMode::Authoritative { .. }) {
            return Ok(());
        }
        if let Some(state) = &self.state {
            ops.save_event_in_cache(&state.event)
                .await
                .context("failed to cache the accepted repository state event")?;
        }
        Ok(())
    }
}

fn all_ref_updates_accepted(ref_updates: &HashMap<String, Option<String>>) -> bool {
    ref_updates.values().all(Option::is_none)
}

fn eligible_git_servers(
    remote_refspecs: HashMap<String, Vec<String>>,
    git_state_refspecs: &[String],
    has_state_event: bool,
    initial_state_relay_results: &[(String, bool)],
    force_policy: &ServerForcePolicy,
) -> (Vec<(String, Vec<String>)>, DroppedRefspecs) {
    let mut eligible = vec![];
    let mut dropped_by_policy = DroppedRefspecs::new();
    for (git_server_url, server_refspecs) in remote_refspecs {
        let server_refspecs = server_refspecs
            .iter()
            .filter(|refspec| {
                // The plan builder may have rewritten a refspec with a
                // leading `+` to force realignment on this server; it
                // still belongs to the same state push and must not be
                // filtered into an apparent no-op plan.
                git_state_refspecs.iter().any(|state_refspec| {
                    state_refspec.trim_start_matches('+') == refspec.trim_start_matches('+')
                })
            })
            .cloned()
            .collect::<Vec<String>>();
        let server_refspecs = if force_policy.allows_force(&git_server_url) {
            server_refspecs
        } else {
            // Fast-forward-only server: destructive refspecs are dropped
            // from the plan and recorded for the caller's reporting.
            let (kept, dropped): (Vec<String>, Vec<String>) = server_refspecs
                .into_iter()
                .partition(|refspec| !is_destructive_refspec(refspec));
            if !dropped.is_empty() {
                dropped_by_policy.insert(git_server_url.clone(), dropped);
            }
            kept
        };
        if is_grasp_server_clone_url(&git_server_url) && has_state_event {
            // Fail closed: git data must not reach a paired GRASP server
            // unless its relay explicitly accepted the staged state event.
            // A missing result — the relay URL could not be derived,
            // staging never produced a result for it, or staging failed
            // outright — counts as rejection, because pushing anyway
            // would create git data whose state the relay will never
            // announce.
            let relay_accepted = format_grasp_server_url_as_relay_url(&git_server_url)
                .ok()
                .is_some_and(|relay_url| {
                    initial_state_relay_results
                        .iter()
                        .any(|(url, succeeded)| *succeeded && relay_urls_match(url, &relay_url))
                });
            if !relay_accepted {
                let short_name = get_short_git_server_name(&git_server_url);
                eprintln!("WARNING: skipping {short_name} - state event failed to reach its relay");
                continue;
            }
        }
        eligible.push((git_server_url, server_refspecs));
    }
    (eligible, dropped_by_policy)
}

fn state_relay_accepted<'a>(
    has_state_event: bool,
    initial_results: impl Iterator<Item = &'a (String, bool)>,
    remaining_results: impl Iterator<Item = &'a (String, bool)>,
) -> bool {
    !has_state_event
        || initial_results
            .chain(remaining_results)
            .any(|(_, succeeded)| *succeeded)
}

/// Publish `events` to the repository relays and the user's write relays,
/// skipping any `excluded_relays` (relays already targeted during GRASP
/// staging).
pub async fn publish_events_to_relays(
    ops: &mut impl StateTransactionOps,
    repo_relays: &[RelayUrl],
    events: Vec<Event>,
    my_write_relays: &[String],
    repo_relay_only: bool,
    excluded_relays: Option<&[RelayUrl]>,
) -> Result<Vec<(String, bool)>> {
    if events.is_empty() {
        return Ok(vec![]);
    }

    let (repo_relays, write_relays) = relay_publish_targets(
        repo_relays,
        my_write_relays,
        repo_relay_only,
        excluded_relays,
    );

    if should_skip_empty_relay_publish(&repo_relays, &write_relays, excluded_relays) {
        return Ok(vec![]);
    }

    ops.publish_events(events, write_relays, repo_relays).await
}

fn relay_publish_targets(
    repo_relays: &[RelayUrl],
    my_write_relays: &[String],
    repo_relay_only: bool,
    excluded_relays: Option<&[RelayUrl]>,
) -> (Vec<RelayUrl>, Vec<String>) {
    let is_excluded = |relay: &str| {
        excluded_relays.is_some_and(|excluded| {
            excluded
                .iter()
                .any(|excluded| relay_urls_match(relay, excluded.as_str()))
        })
    };

    let repo_relays = repo_relays
        .iter()
        .filter(|relay| !is_excluded(relay.as_str()))
        .cloned()
        .collect::<Vec<RelayUrl>>();
    let write_relays = if repo_relay_only {
        vec![]
    } else {
        my_write_relays
            .iter()
            .filter(|relay| !is_excluded(relay))
            .cloned()
            .collect::<Vec<String>>()
    };

    (repo_relays, write_relays)
}

fn should_skip_empty_relay_publish(
    repo_relays: &[RelayUrl],
    write_relays: &[String],
    excluded_relays: Option<&[RelayUrl]>,
) -> bool {
    let excluded_any_relays = excluded_relays.is_some_and(|relays| !relays.is_empty());
    excluded_any_relays && repo_relays.is_empty() && write_relays.is_empty()
}

fn relay_urls_match(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use ngit::client::STATE_KIND;
    use nostr::{
        EventBuilder, EventId, Keys, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
        nips::nip19::ToBech32,
    };

    use super::*;

    const MAIN_OID: &str = "1111111111111111111111111111111111111111";

    fn signed_state_event(refs: &[(&str, &str)]) -> Event {
        let keys = Keys::generate();
        let mut tags = vec![Tag::identifier("test-repo")];
        for (name, value) in refs {
            tags.push(Tag::parse([*name, *value]).unwrap());
        }
        keys.sign_event(
            EventBuilder::new(STATE_KIND, "")
                .tags(tags)
                .finalize_unsigned(keys.public_key()),
        )
        .unwrap()
    }

    /// Build a [`RepoState`] through the production parser.
    fn repo_state(refs: &[(&str, &str)]) -> RepoState {
        RepoState::try_from(vec![signed_state_event(refs)]).unwrap()
    }

    fn main_state() -> RepoState {
        repo_state(&[("refs/heads/main", MAIN_OID)])
    }

    fn test_repo_ref(git_server: Vec<String>, relays: Vec<&str>) -> RepoRef {
        let public_key = Keys::generate().public_key();
        RepoRef {
            identifier: "test-repo".to_string(),
            name: "test".to_string(),
            description: String::new(),
            root_commit: "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2".to_string(),
            git_server,
            web: vec![],
            upstream: vec![],
            relays: relays
                .iter()
                .map(|relay| RelayUrl::parse(relay).unwrap())
                .collect(),
            blossoms: vec![],
            hashtags: vec![],
            maintainers: vec![public_key],
            selected_maintainer: public_key,
            maintainers_without_annoucnement: None,
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: vec![],
        }
    }

    fn grasp_clone_url(host: &str) -> String {
        let npub = Keys::generate().public_key().to_bech32().unwrap();
        format!("https://{host}/{npub}/test-repo.git")
    }

    fn refspec() -> String {
        "refs/heads/main:refs/heads/main".to_string()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum FakeCall {
        /// caching publication (`publish_events`) — other events only
        Publish {
            event_ids: Vec<EventId>,
            my_write_relays: Vec<String>,
            repo_relays: Vec<String>,
        },
        /// non-caching publication (`publish_state_events`) — the state
        /// candidate during staging and fanout
        PublishState {
            event_ids: Vec<EventId>,
            my_write_relays: Vec<String>,
            repo_relays: Vec<String>,
        },
        Push {
            git_server_url: String,
            refspecs: Vec<String>,
        },
        SaveToCache(EventId),
    }

    enum FakePushResult {
        Refs(HashMap<String, Option<String>>),
        ConnectionError,
    }

    /// Deterministic [`StateTransactionOps`] that records every call.
    /// Relays missing from `relay_acceptance` accept; relays listed in
    /// `omit_relay_results` produce no per-relay result at all; git
    /// servers missing from `push_results` fail with a connection error.
    #[derive(Default)]
    struct FakeOps {
        relay_acceptance: HashMap<String, bool>,
        omit_relay_results: Vec<String>,
        publish_error: bool,
        push_results: HashMap<String, FakePushResult>,
        calls: Vec<FakeCall>,
    }

    impl FakeOps {
        fn accepts(&self, relay: &str) -> bool {
            *self
                .relay_acceptance
                .get(relay.trim_end_matches('/'))
                .unwrap_or(&true)
        }

        fn publish_calls(&self) -> Vec<&FakeCall> {
            self.calls
                .iter()
                .filter(|call| {
                    matches!(
                        call,
                        FakeCall::Publish { .. } | FakeCall::PublishState { .. }
                    )
                })
                .collect()
        }

        fn relay_results(
            &self,
            my_write_relays: &[String],
            repo_relays: &[String],
        ) -> Result<Vec<(String, bool)>> {
            if self.publish_error {
                anyhow::bail!("relay publish failed")
            }
            // like the real send_events, report results against
            // trailing-slash-trimmed relay urls
            Ok(repo_relays
                .iter()
                .chain(my_write_relays.iter())
                .map(|relay| {
                    let relay = relay.trim_end_matches('/').to_string();
                    let accepted = self.accepts(&relay);
                    (relay, accepted)
                })
                .filter(|(relay, _)| !self.omit_relay_results.contains(relay))
                .collect())
        }

        fn pushed_servers(&self) -> Vec<&str> {
            self.calls
                .iter()
                .filter_map(|call| match call {
                    FakeCall::Push { git_server_url, .. } => Some(git_server_url.as_str()),
                    _ => None,
                })
                .collect()
        }
    }

    impl StateTransactionOps for FakeOps {
        async fn publish_events(
            &mut self,
            events: Vec<Event>,
            my_write_relays: Vec<String>,
            repo_relays: Vec<RelayUrl>,
        ) -> Result<Vec<(String, bool)>> {
            let repo_relays: Vec<String> = repo_relays.iter().map(ToString::to_string).collect();
            self.calls.push(FakeCall::Publish {
                event_ids: events.iter().map(|event| event.id).collect(),
                my_write_relays: my_write_relays.clone(),
                repo_relays: repo_relays.clone(),
            });
            self.relay_results(&my_write_relays, &repo_relays)
        }

        async fn publish_state_events(
            &mut self,
            events: Vec<Event>,
            my_write_relays: Vec<String>,
            repo_relays: Vec<RelayUrl>,
        ) -> Result<Vec<(String, bool)>> {
            let repo_relays: Vec<String> = repo_relays.iter().map(ToString::to_string).collect();
            self.calls.push(FakeCall::PublishState {
                event_ids: events.iter().map(|event| event.id).collect(),
                my_write_relays: my_write_relays.clone(),
                repo_relays: repo_relays.clone(),
            });
            self.relay_results(&my_write_relays, &repo_relays)
        }

        fn push_to_git_server(
            &mut self,
            git_server_url: &str,
            refspecs: &[String],
        ) -> Result<HashMap<String, Option<String>>> {
            self.calls.push(FakeCall::Push {
                git_server_url: git_server_url.to_string(),
                refspecs: refspecs.to_vec(),
            });
            match self.push_results.get(git_server_url) {
                Some(FakePushResult::Refs(ref_updates)) => Ok(ref_updates.clone()),
                Some(FakePushResult::ConnectionError) | None => {
                    Err(anyhow::anyhow!("connection failed"))
                }
            }
        }

        async fn save_event_in_cache(&mut self, event: &Event) -> Result<()> {
            self.calls.push(FakeCall::SaveToCache(event.id));
            Ok(())
        }
    }

    mod publish_state_to_grasps_first {
        use super::*;

        #[tokio::test]
        async fn no_state_event_skips_grasp_staging() {
            let repo_ref = test_repo_ref(vec![grasp_clone_url("grasp.example")], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, None);

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn no_grasp_servers_skips_staging() {
            let repo_ref = test_repo_ref(
                vec!["https://vanilla.example/repo.git".to_string()],
                vec!["wss://relay.example"],
            );
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn stages_state_on_grasp_relays_only() {
            let repo_ref = test_repo_ref(vec![grasp_clone_url("grasp.example")], vec![]);
            let state = main_state();
            let event = state.event.clone();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(state));

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert_eq!(
                ops.calls,
                vec![FakeCall::PublishState {
                    event_ids: vec![event.id],
                    my_write_relays: vec![],
                    repo_relays: vec!["wss://grasp.example".to_string()],
                }]
            );
        }

        #[tokio::test]
        async fn staging_dedupes_grasp_relays_sharing_a_host() {
            let repo_ref = test_repo_ref(
                vec![
                    grasp_clone_url("grasp.example"),
                    grasp_clone_url("grasp.example"),
                ],
                vec![],
            );
            let state = main_state();
            let event = state.event.clone();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(state));

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert_eq!(
                ops.calls,
                vec![FakeCall::PublishState {
                    event_ids: vec![event.id],
                    my_write_relays: vec![],
                    repo_relays: vec!["wss://grasp.example".to_string()],
                }]
            );
        }
    }

    mod push_git_state_refspecs {
        use super::*;

        fn successful_push() -> FakePushResult {
            FakePushResult::Refs(HashMap::new())
        }

        fn rejected_push() -> FakePushResult {
            FakePushResult::Refs(HashMap::from([(
                "refs/heads/main".to_string(),
                Some("hook declined".to_string()),
            )]))
        }

        #[tokio::test]
        async fn no_state_refspecs_is_vacuously_accepted() {
            let repo_ref = test_repo_ref(vec![grasp_clone_url("grasp.example")], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, None);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(&mut ops, HashMap::new(), &[]);

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn skips_grasp_server_whose_relay_rejected_state_but_keeps_vanilla() {
            let grasp_url = grasp_clone_url("grasp.example");
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![grasp_url.clone(), vanilla_url.clone()], vec![]);
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://grasp.example".to_string(), false)]),
                push_results: HashMap::from([(vanilla_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (grasp_url, vec![refspec()]),
                    (vanilla_url.clone(), vec![refspec()]),
                ]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(ops.pushed_servers(), vec![vanilla_url.as_str()]);
        }

        #[tokio::test]
        async fn no_eligible_servers_when_only_grasp_and_its_relay_rejected_state() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(vec![grasp_url.clone()], vec![]);
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://grasp.example".to_string(), false)]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::NoEligibleServers));
            assert!(ops.pushed_servers().is_empty());
        }

        #[tokio::test]
        async fn grasp_server_remains_eligible_when_its_relay_accepted_state() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(vec![grasp_url.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([(grasp_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(grasp_url.clone(), vec![refspec()])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(ops.pushed_servers(), vec![grasp_url.as_str()]);
        }

        #[tokio::test]
        async fn grasp_server_eligible_without_state_event() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(vec![grasp_url.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([(grasp_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, None);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(grasp_url.clone(), vec![refspec()])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(ops.pushed_servers(), vec![grasp_url.as_str()]);
        }

        #[tokio::test]
        async fn all_pushes_failed_when_every_server_errors_or_rejects_a_ref() {
            let vanilla_a = "https://a.example/repo.git".to_string();
            let vanilla_b = "https://b.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_a.clone(), vanilla_b.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([
                    (vanilla_a.clone(), FakePushResult::ConnectionError),
                    (vanilla_b.clone(), rejected_push()),
                ]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_a, vec![refspec()]), (vanilla_b, vec![refspec()])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AllPushesFailed));
            assert_eq!(ops.pushed_servers().len(), 2);
        }

        #[tokio::test]
        async fn server_with_empty_plan_counts_as_success_without_pushing() {
            // Replaces server_with_no_matching_refspecs_counts_as_failed_
            // without_pushing: an empty plan means every requested change
            // is already applied on the server, so instead of counting it
            // as a failed push the server counts as success without being
            // pushed to.
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_url.clone(), vec![])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert!(ops.pushed_servers().is_empty());
        }

        #[tokio::test]
        async fn empty_plan_counts_as_success_without_state_event() {
            // The no-op semantics also apply under nostr.nostate: a server
            // that already has every requested change is a success even
            // though no state event is being published.
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, None);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_url.clone(), vec![])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert!(ops.pushed_servers().is_empty());
        }

        #[tokio::test]
        async fn force_rewritten_plan_refspec_is_still_pushed() {
            // The plan builder rewrites a refspec to `+<refspec>` when a
            // server needs forced realignment; the rewritten entry must
            // still count as part of the state push instead of being
            // filtered into an apparent no-op plan that would count the
            // server as an unearned success.
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let forced_refspec = format!("+{}", refspec());
            let mut ops = FakeOps {
                push_results: HashMap::from([(vanilla_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_url.clone(), vec![forced_refspec.clone()])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(
                ops.calls
                    .iter()
                    .filter_map(|call| match call {
                        FakeCall::Push { refspecs, .. } => Some(refspecs.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec![vec![forced_refspec]]
            );
        }

        #[tokio::test]
        async fn already_applied_deletion_counts_as_success() {
            // Deleting a branch that is already absent from the server
            // produces an empty per-server plan, so the push succeeds
            // without contacting the server.
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let delete_refspec = ":refs/heads/gone".to_string();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_url.clone(), vec![])]),
                &[delete_refspec],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert!(ops.pushed_servers().is_empty());
        }

        #[test]
        fn grasp_server_ineligible_when_staging_returned_no_results() {
            // Replaces the fail-open characterization
            // `grasp_server_eligible_when_staging_returned_no_results`: a
            // total staging failure must not let git data through to a
            // paired GRASP server whose relay never accepted the state.
            let grasp_url = grasp_clone_url("grasp.example");

            let (servers, _) = super::super::eligible_git_servers(
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
                true,
                &[],
                &ServerForcePolicy::ForceRealignAll,
            );

            assert!(servers.is_empty());
        }

        #[test]
        fn grasp_server_ineligible_when_paired_relay_result_is_missing() {
            let grasp_url = grasp_clone_url("grasp.example");

            let (servers, _) = super::super::eligible_git_servers(
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
                true,
                // staging produced results, but none for this server's
                // paired relay
                &[("wss://other.example".to_string(), true)],
                &ServerForcePolicy::ForceRealignAll,
            );

            assert!(servers.is_empty());
        }

        #[tokio::test]
        async fn total_staging_failure_skips_grasp_but_keeps_vanilla_server() {
            let grasp_url = grasp_clone_url("grasp.example");
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![grasp_url.clone(), vanilla_url.clone()], vec![]);
            let mut ops = FakeOps {
                publish_error: true,
                push_results: HashMap::from([(vanilla_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (grasp_url, vec![refspec()]),
                    (vanilla_url.clone(), vec![refspec()]),
                ]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(ops.pushed_servers(), vec![vanilla_url.as_str()]);
        }

        #[tokio::test]
        async fn mixed_grasp_acceptance_skips_only_the_rejected_relays_pair() {
            let grasp_a = grasp_clone_url("a.example");
            let grasp_b = grasp_clone_url("b.example");
            let repo_ref = test_repo_ref(vec![grasp_a.clone(), grasp_b.clone()], vec![]);
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://b.example".to_string(), false)]),
                push_results: HashMap::from([(grasp_a.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (grasp_a.clone(), vec![refspec()]),
                    (grasp_b, vec![refspec()]),
                ]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(ops.pushed_servers(), vec![grasp_a.as_str()]);
        }

        #[tokio::test]
        async fn accepted_when_at_least_one_server_push_succeeds() {
            let vanilla_a = "https://a.example/repo.git".to_string();
            let vanilla_b = "https://b.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_a.clone(), vanilla_b.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([
                    (vanilla_a.clone(), FakePushResult::ConnectionError),
                    (vanilla_b.clone(), successful_push()),
                ]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (vanilla_a.clone(), vec![refspec()]),
                    (vanilla_b.clone(), vec![refspec()]),
                ]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
        }
    }

    mod server_force_policy {
        use super::*;

        fn successful_push() -> FakePushResult {
            FakePushResult::Refs(HashMap::new())
        }

        fn forced_refspec() -> String {
            "+refs/heads/diverged:refs/heads/diverged".to_string()
        }

        fn delete_refspec() -> String {
            ":refs/heads/gone".to_string()
        }

        fn state_refspecs() -> Vec<String> {
            vec![
                refspec(),
                "refs/heads/diverged:refs/heads/diverged".to_string(),
                delete_refspec(),
            ]
        }

        #[test]
        fn force_only_on_matches_ignoring_trailing_slash() {
            let policy =
                ServerForcePolicy::ForceOnlyOn(vec!["https://grasp.example/repo.git/".to_string()]);

            assert!(policy.allows_force("https://grasp.example/repo.git"));
            assert!(!policy.allows_force("https://vanilla.example/repo.git"));
        }

        #[tokio::test]
        async fn fast_forward_only_server_drops_destructive_refspecs_but_forceable_keeps_them() {
            let grasp_url = grasp_clone_url("grasp.example");
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![grasp_url.clone(), vanilla_url.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([
                    (grasp_url.clone(), successful_push()),
                    (vanilla_url.clone(), successful_push()),
                ]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()))
                .with_force_policy(ServerForcePolicy::ForceOnlyOn(vec![grasp_url.clone()]));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let plan = vec![refspec(), forced_refspec(), delete_refspec()];
            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (grasp_url.clone(), plan.clone()),
                    (vanilla_url.clone(), plan),
                ]),
                &state_refspecs(),
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            let pushed: HashMap<String, Vec<String>> = ops
                .calls
                .iter()
                .filter_map(|call| match call {
                    FakeCall::Push {
                        git_server_url,
                        refspecs,
                    } => Some((git_server_url.clone(), refspecs.clone())),
                    _ => None,
                })
                .collect();
            assert_eq!(
                pushed.get(&grasp_url),
                Some(&vec![refspec(), forced_refspec(), delete_refspec()])
            );
            assert_eq!(pushed.get(&vanilla_url), Some(&vec![refspec()]));
            assert_eq!(
                transaction.refspecs_dropped_by_policy(),
                &HashMap::from([(vanilla_url, vec![forced_refspec(), delete_refspec()])])
            );
        }

        #[tokio::test]
        async fn fully_dropped_plan_counts_as_success_without_push() {
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()))
                .with_force_policy(ServerForcePolicy::ForceOnlyOn(vec![]));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(
                    vanilla_url.clone(),
                    vec![forced_refspec(), delete_refspec()],
                )]),
                &state_refspecs(),
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert!(ops.pushed_servers().is_empty());
            assert_eq!(
                transaction.refspecs_dropped_by_policy(),
                &HashMap::from([(vanilla_url, vec![forced_refspec(), delete_refspec()])])
            );
        }

        #[tokio::test]
        async fn default_policy_keeps_destructive_refspecs_on_vanilla_servers() {
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([(vanilla_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(
                    vanilla_url.clone(),
                    vec![forced_refspec(), delete_refspec()],
                )]),
                &state_refspecs(),
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(
                ops.calls
                    .iter()
                    .filter_map(|call| match call {
                        FakeCall::Push { refspecs, .. } => Some(refspecs.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec![vec![forced_refspec(), delete_refspec()]]
            );
            assert!(transaction.refspecs_dropped_by_policy().is_empty());
        }
    }

    mod server_push_outcomes {
        use super::*;

        fn successful_push() -> FakePushResult {
            FakePushResult::Refs(HashMap::new())
        }

        fn rejected_push() -> FakePushResult {
            FakePushResult::Refs(HashMap::from([(
                "refs/heads/main".to_string(),
                Some("hook declined".to_string()),
            )]))
        }

        #[tokio::test]
        async fn records_one_outcome_per_pushed_or_in_sync_server() {
            let accepted_url = "https://accepted.example/repo.git".to_string();
            let rejected_url = "https://rejected.example/repo.git".to_string();
            let failed_url = "https://failed.example/repo.git".to_string();
            let in_sync_url = "https://in-sync.example/repo.git".to_string();
            let repo_ref = test_repo_ref(
                vec![
                    accepted_url.clone(),
                    rejected_url.clone(),
                    failed_url.clone(),
                    in_sync_url.clone(),
                ],
                vec![],
            );
            let mut ops = FakeOps {
                push_results: HashMap::from([
                    (accepted_url.clone(), successful_push()),
                    (rejected_url.clone(), rejected_push()),
                    (failed_url.clone(), FakePushResult::ConnectionError),
                ]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (accepted_url.clone(), vec![refspec()]),
                    (rejected_url.clone(), vec![refspec()]),
                    (failed_url.clone(), vec![refspec()]),
                    (in_sync_url.clone(), vec![]),
                ]),
                &[refspec()],
            );

            assert_eq!(
                transaction.server_push_outcomes(),
                &HashMap::from([
                    (accepted_url, ServerPushOutcome::Accepted),
                    (rejected_url, ServerPushOutcome::RefsRejected),
                    (failed_url, ServerPushOutcome::Failed),
                    (in_sync_url, ServerPushOutcome::AlreadyApplied),
                ])
            );
        }

        #[tokio::test]
        async fn gate_skipped_grasp_server_records_no_outcome() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(vec![grasp_url.clone()], vec![]);
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://grasp.example".to_string(), false)]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
            );

            assert!(transaction.server_push_outcomes().is_empty());
        }

        #[tokio::test]
        async fn policy_dropped_plan_records_already_applied() {
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let forced_refspec = "+refs/heads/diverged:refs/heads/diverged".to_string();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()))
                .with_force_policy(ServerForcePolicy::ForceOnlyOn(vec![]));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_url.clone(), vec![forced_refspec.clone()])]),
                &[forced_refspec],
            );

            assert_eq!(
                transaction.server_push_outcomes(),
                &HashMap::from([(vanilla_url, ServerPushOutcome::AlreadyApplied)])
            );
        }
    }

    mod authoritative_mode {
        use super::*;

        fn successful_push() -> FakePushResult {
            FakePushResult::Refs(HashMap::new())
        }

        fn relay(url: &str) -> RelayUrl {
            RelayUrl::parse(url).unwrap()
        }

        #[tokio::test]
        async fn seeds_only_grasp_relays_missing_the_event() {
            let grasp_a = grasp_clone_url("a.example");
            let grasp_b = grasp_clone_url("b.example");
            let repo_ref = test_repo_ref(vec![grasp_a.clone(), grasp_b.clone()], vec![]);
            let state = main_state();
            let event = state.event.clone();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new_authoritative(
                &repo_ref,
                state,
                vec![relay("wss://a.example")],
            );

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert_eq!(
                ops.calls,
                vec![FakeCall::PublishState {
                    event_ids: vec![event.id],
                    my_write_relays: vec![],
                    repo_relays: vec!["wss://b.example".to_string()],
                }]
            );
        }

        #[tokio::test]
        async fn all_relays_holding_means_no_publish_and_all_servers_eligible() {
            let grasp_a = grasp_clone_url("a.example");
            let grasp_b = grasp_clone_url("b.example");
            let repo_ref = test_repo_ref(vec![grasp_a.clone(), grasp_b.clone()], vec![]);
            let mut ops = FakeOps {
                push_results: HashMap::from([
                    (grasp_a.clone(), successful_push()),
                    (grasp_b.clone(), successful_push()),
                ]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new_authoritative(
                &repo_ref,
                main_state(),
                vec![relay("wss://a.example"), relay("wss://b.example")],
            );
            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert!(ops.publish_calls().is_empty());

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (grasp_a.clone(), vec![refspec()]),
                    (grasp_b.clone(), vec![refspec()]),
                ]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            let mut pushed = ops.pushed_servers();
            pushed.sort_unstable();
            let mut expected = vec![grasp_a.as_str(), grasp_b.as_str()];
            expected.sort_unstable();
            assert_eq!(pushed, expected);
        }

        #[tokio::test]
        async fn seeding_rejection_still_fails_closed_for_that_server() {
            let grasp_a = grasp_clone_url("a.example");
            let grasp_b = grasp_clone_url("b.example");
            let repo_ref = test_repo_ref(vec![grasp_a.clone(), grasp_b.clone()], vec![]);
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://b.example".to_string(), false)]),
                push_results: HashMap::from([(grasp_a.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction = StateTransaction::new_authoritative(
                &repo_ref,
                main_state(),
                vec![relay("wss://a.example")],
            );
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([
                    (grasp_a.clone(), vec![refspec()]),
                    (grasp_b, vec![refspec()]),
                ]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            assert_eq!(ops.pushed_servers(), vec![grasp_a.as_str()]);
        }

        #[tokio::test]
        async fn no_cache_write_through_the_whole_flow() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(
                vec![grasp_url.clone()],
                vec!["wss://grasp.example", "wss://other.relay"],
            );
            let mut ops = FakeOps {
                push_results: HashMap::from([(grasp_url.clone(), successful_push())]),
                ..Default::default()
            };
            let mut transaction =
                StateTransaction::new_authoritative(&repo_ref, main_state(), vec![]);
            transaction.publish_state_to_grasps_first(&mut ops).await;
            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
            );
            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            transaction
                .publish_state_to_remaining_relays(
                    &mut ops,
                    &["wss://write.relay".to_string()],
                    false,
                )
                .await
                .unwrap();

            assert!(transaction.state_relay_accepted());
            transaction.commit(&mut ops).await.unwrap();

            assert!(
                !ops.calls
                    .iter()
                    .any(|call| matches!(call, FakeCall::SaveToCache(_)))
            );
        }

        #[tokio::test]
        async fn fanout_to_remaining_relays_is_skipped() {
            let repo_ref = test_repo_ref(vec![], vec!["wss://repo.relay"]);
            let mut ops = FakeOps::default();
            let mut transaction =
                StateTransaction::new_authoritative(&repo_ref, main_state(), vec![]);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction
                .publish_state_to_remaining_relays(
                    &mut ops,
                    &["wss://write.relay".to_string()],
                    false,
                )
                .await
                .unwrap();

            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn relay_acceptance_is_vacuous_without_grasp_relays() {
            // Plain sync propagates canonical state to vanilla servers
            // without publishing anything; the relay-acceptance gate must
            // not fail the transaction.
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction =
                StateTransaction::new_authoritative(&repo_ref, main_state(), vec![]);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert!(ops.calls.is_empty());
            assert!(transaction.state_relay_accepted());
        }
    }

    mod publish_state_to_remaining_relays {
        use super::*;

        #[tokio::test]
        async fn no_state_event_publishes_nothing() {
            let repo_ref = test_repo_ref(vec![], vec!["wss://relay.example"]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, None);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction
                .publish_state_to_remaining_relays(&mut ops, &[], false)
                .await
                .unwrap();

            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn fanout_excludes_initially_targeted_grasp_relays() {
            let repo_ref = test_repo_ref(
                vec![grasp_clone_url("grasp.example")],
                vec!["wss://grasp.example", "wss://other.relay"],
            );
            let state = main_state();
            let event = state.event.clone();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(state));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction
                .publish_state_to_remaining_relays(
                    &mut ops,
                    &[
                        "wss://grasp.example".to_string(),
                        "wss://write.relay".to_string(),
                    ],
                    false,
                )
                .await
                .unwrap();

            assert_eq!(
                ops.publish_calls(),
                vec![
                    &FakeCall::PublishState {
                        event_ids: vec![event.id],
                        my_write_relays: vec![],
                        repo_relays: vec!["wss://grasp.example".to_string()],
                    },
                    &FakeCall::PublishState {
                        event_ids: vec![event.id],
                        my_write_relays: vec!["wss://write.relay".to_string()],
                        repo_relays: vec!["wss://other.relay".to_string()],
                    },
                ]
            );
        }

        #[tokio::test]
        async fn repo_relay_only_drops_write_relays_from_fanout() {
            let repo_ref = test_repo_ref(vec![], vec!["wss://relay.example"]);
            let state = main_state();
            let event = state.event.clone();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(state));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction
                .publish_state_to_remaining_relays(
                    &mut ops,
                    &["wss://write.relay".to_string()],
                    true,
                )
                .await
                .unwrap();

            assert_eq!(
                ops.calls,
                vec![FakeCall::PublishState {
                    event_ids: vec![event.id],
                    my_write_relays: vec![],
                    repo_relays: vec!["wss://relay.example".to_string()],
                }]
            );
        }

        #[tokio::test]
        async fn skips_send_when_exclusions_leave_no_relays() {
            let repo_ref = test_repo_ref(
                vec![grasp_clone_url("grasp.example")],
                vec!["wss://grasp.example"],
            );
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(&mut ops).await;

            transaction
                .publish_state_to_remaining_relays(&mut ops, &[], false)
                .await
                .unwrap();

            // only the staging publish; no remaining-relay send that would
            // otherwise fall back to default relays
            assert_eq!(ops.publish_calls().len(), 1);
        }
    }

    mod state_relay_accepted {
        use super::*;

        async fn run_transaction(
            repo_ref: &RepoRef,
            state: Option<RepoState>,
            ops: &mut FakeOps,
            my_write_relays: &[String],
        ) -> bool {
            let mut transaction = StateTransaction::new(repo_ref, state);
            transaction.publish_state_to_grasps_first(ops).await;
            transaction
                .publish_state_to_remaining_relays(ops, my_write_relays, false)
                .await
                .unwrap();
            transaction.state_relay_accepted()
        }

        #[tokio::test]
        async fn vacuously_accepted_without_state_event_or_relay_results() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let mut ops = FakeOps::default();

            assert!(run_transaction(&repo_ref, None, &mut ops, &[]).await);
        }

        #[tokio::test]
        async fn not_accepted_when_state_event_present_but_no_relay_responded() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let mut ops = FakeOps::default();

            assert!(!run_transaction(&repo_ref, Some(main_state()), &mut ops, &[]).await);
        }

        #[tokio::test]
        async fn accepted_when_only_initial_grasp_staging_succeeded() {
            let repo_ref = test_repo_ref(
                vec![grasp_clone_url("grasp.example")],
                vec!["wss://other.relay"],
            );
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://other.relay".to_string(), false)]),
                ..Default::default()
            };

            assert!(run_transaction(&repo_ref, Some(main_state()), &mut ops, &[]).await);
        }

        #[tokio::test]
        async fn accepted_when_only_remaining_fanout_succeeded() {
            let repo_ref = test_repo_ref(
                vec![grasp_clone_url("grasp.example")],
                vec!["wss://other.relay"],
            );
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://grasp.example".to_string(), false)]),
                ..Default::default()
            };

            assert!(run_transaction(&repo_ref, Some(main_state()), &mut ops, &[]).await);
        }

        #[tokio::test]
        async fn not_accepted_when_every_relay_rejected_the_state_event() {
            let repo_ref = test_repo_ref(
                vec![grasp_clone_url("grasp.example")],
                vec!["wss://other.relay"],
            );
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([
                    ("wss://grasp.example".to_string(), false),
                    ("wss://other.relay".to_string(), false),
                ]),
                ..Default::default()
            };

            assert!(!run_transaction(&repo_ref, Some(main_state()), &mut ops, &[]).await);
        }
    }

    mod commit {
        use super::*;

        /// Drive the full happy path against a single accepting GRASP
        /// pair and return the transaction ready for
        /// [`StateTransaction::commit`].
        async fn run_accepted_push<'a>(
            repo_ref: &'a RepoRef,
            grasp_url: &str,
            ops: &mut FakeOps,
        ) -> StateTransaction<'a> {
            let mut transaction = StateTransaction::new(repo_ref, Some(main_state()));
            transaction.publish_state_to_grasps_first(ops).await;
            let outcome = transaction.push_git_state_refspecs(
                ops,
                HashMap::from([(grasp_url.to_string(), vec![refspec()])]),
                &[refspec()],
            );
            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
            transaction
                .publish_state_to_remaining_relays(ops, &[], false)
                .await
                .unwrap();
            transaction
        }

        #[tokio::test]
        async fn candidate_is_cached_exactly_once_and_only_at_the_commit_point() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(
                vec![grasp_url.clone()],
                vec!["wss://grasp.example", "wss://other.relay"],
            );
            let mut ops = FakeOps {
                push_results: HashMap::from([(
                    grasp_url.clone(),
                    FakePushResult::Refs(HashMap::new()),
                )]),
                ..Default::default()
            };

            let transaction = run_accepted_push(&repo_ref, &grasp_url, &mut ops).await;

            // staging, push and fanout must not have touched the cache
            assert!(
                !ops.calls
                    .iter()
                    .any(|call| matches!(call, FakeCall::SaveToCache(_)))
            );

            assert!(transaction.state_relay_accepted());
            transaction.commit(&mut ops).await.unwrap();

            let expected_id = transaction.state.as_ref().unwrap().event.id;
            let cached: Vec<_> = ops
                .calls
                .iter()
                .filter(|call| matches!(call, FakeCall::SaveToCache(_)))
                .collect();
            assert_eq!(cached, vec![&FakeCall::SaveToCache(expected_id)]);
        }

        #[tokio::test]
        async fn staging_and_fanout_use_non_caching_publication() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(
                vec![grasp_url.clone()],
                vec!["wss://grasp.example", "wss://other.relay"],
            );
            let mut ops = FakeOps {
                push_results: HashMap::from([(
                    grasp_url.clone(),
                    FakePushResult::Refs(HashMap::new()),
                )]),
                ..Default::default()
            };

            run_accepted_push(&repo_ref, &grasp_url, &mut ops).await;

            // both publish calls (staging + fanout) went through the
            // non-caching path
            assert_eq!(ops.publish_calls().len(), 2);
            assert!(
                ops.calls
                    .iter()
                    .filter(|call| matches!(
                        call,
                        FakeCall::Publish { .. } | FakeCall::PublishState { .. }
                    ))
                    .all(|call| matches!(call, FakeCall::PublishState { .. }))
            );
        }

        #[tokio::test]
        async fn grasp_staging_acceptance_with_failed_fanout_still_commits() {
            let grasp_url = grasp_clone_url("grasp.example");
            let repo_ref = test_repo_ref(
                vec![grasp_url.clone()],
                vec!["wss://grasp.example", "wss://other.relay"],
            );
            let mut ops = FakeOps {
                relay_acceptance: HashMap::from([("wss://other.relay".to_string(), false)]),
                push_results: HashMap::from([(
                    grasp_url.clone(),
                    FakePushResult::Refs(HashMap::new()),
                )]),
                ..Default::default()
            };

            let transaction = run_accepted_push(&repo_ref, &grasp_url, &mut ops).await;

            // GRASP staging acceptance alone satisfies relay acceptance
            assert!(transaction.state_relay_accepted());
            transaction.commit(&mut ops).await.unwrap();
            assert!(
                ops.calls
                    .iter()
                    .any(|call| matches!(call, FakeCall::SaveToCache(_)))
            );
        }

        #[tokio::test]
        async fn commit_without_a_candidate_state_does_nothing() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let mut ops = FakeOps::default();
            let transaction = StateTransaction::new(&repo_ref, None);

            transaction.commit(&mut ops).await.unwrap();

            assert!(ops.calls.is_empty());
        }
    }

    mod user_message {
        use super::*;

        #[test]
        fn failure_reasons_map_to_the_remote_helper_error_text() {
            assert_eq!(
                StateTransactionFailure::NoEligibleGitServers.user_message(),
                "state event failed to reach any git server relay"
            );
            assert_eq!(
                StateTransactionFailure::AllGitServerPushesFailed.user_message(),
                "failed to push to any git server"
            );
            assert_eq!(
                StateTransactionFailure::StateNotAcceptedByAnyRelay.user_message(),
                "state event failed to reach any relay"
            );
        }
    }

    mod all_ref_updates_accepted {
        use super::*;

        #[test]
        fn accepts_empty_or_successful_statuses() {
            assert!(super::super::all_ref_updates_accepted(&HashMap::new()));
            assert!(super::super::all_ref_updates_accepted(&HashMap::from([
                ("refs/heads/main".to_string(), None),
                ("refs/tags/v1".to_string(), None),
            ])));
        }

        #[test]
        fn rejects_any_failed_status() {
            assert!(!super::super::all_ref_updates_accepted(&HashMap::from([
                ("refs/heads/main".to_string(), None),
                (
                    "refs/tags/v1".to_string(),
                    Some("hook declined".to_string()),
                ),
            ])));
        }
    }

    mod relay_publish_targets {
        use super::*;

        #[test]
        fn excludes_grasp_relays_and_returns_empty_when_none_remain() {
            let grasp_relay = RelayUrl::parse("ws://grasp.example").unwrap();
            let (repo_relays, write_relays) = super::super::relay_publish_targets(
                std::slice::from_ref(&grasp_relay),
                &[],
                false,
                Some(std::slice::from_ref(&grasp_relay)),
            );

            assert!(repo_relays.is_empty());
            assert!(write_relays.is_empty());
            assert!(super::super::should_skip_empty_relay_publish(
                &repo_relays,
                &write_relays,
                Some(std::slice::from_ref(&grasp_relay)),
            ));
        }

        #[test]
        fn excludes_matching_write_relays_before_fallback_decision() {
            let excluded = RelayUrl::parse("ws://grasp.example").unwrap();
            let write_relays = vec!["ws://grasp.example/".to_string()];
            let (repo_relays, write_relays) = super::super::relay_publish_targets(
                &[],
                &write_relays,
                false,
                Some(std::slice::from_ref(&excluded)),
            );

            assert!(repo_relays.is_empty());
            assert!(write_relays.is_empty());
            assert!(super::super::should_skip_empty_relay_publish(
                &repo_relays,
                &write_relays,
                Some(std::slice::from_ref(&excluded)),
            ));
        }

        #[test]
        fn allows_send_events_fallback_when_no_grasp_relays_were_excluded() {
            let (repo_relays, write_relays) =
                super::super::relay_publish_targets(&[], &[], false, Some(&[]));

            assert!(repo_relays.is_empty());
            assert!(write_relays.is_empty());
            assert!(!super::super::should_skip_empty_relay_publish(
                &repo_relays,
                &write_relays,
                Some(&[]),
            ));
            assert!(!super::super::should_skip_empty_relay_publish(
                &repo_relays,
                &write_relays,
                None,
            ));
        }

        #[test]
        fn keeps_non_excluded_relays() {
            let excluded = RelayUrl::parse("ws://grasp.example").unwrap();
            let repo_relay = RelayUrl::parse("ws://relay.example").unwrap();
            let write_relays = vec!["ws://write.example".to_string()];
            let (repo_relays, write_relays) = super::super::relay_publish_targets(
                std::slice::from_ref(&repo_relay),
                &write_relays,
                false,
                Some(std::slice::from_ref(&excluded)),
            );

            assert_eq!(repo_relays, vec![repo_relay]);
            assert_eq!(write_relays, vec!["ws://write.example".to_string()]);
        }
    }
}
