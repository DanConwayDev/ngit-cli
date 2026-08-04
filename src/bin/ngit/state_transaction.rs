//! The repository-state publication transaction.
//!
//! A normal `git push` to the nostr remote publishes an updated repository
//! state event alongside the git data it references. GRASP relays that
//! support purgatory hold repo state events until the matching git data
//! arrives at the paired git server. Publishing to them before the git
//! push is therefore safe: if the later git push fails, the relay never
//! broadcasts the unreachable state.
//!
//! This intentionally makes state publication a two-stage process. First
//! seed purgatory on the GRASP relays we are about to push to, then push
//! git data, and only after at least one git server accepts the data fan
//! out the state to any remaining relays. The extra round-trip prevents
//! us reporting `ok` or broadcasting state for commits that no git server
//! has.
//!
//! [`StateTransaction`] owns those phases plus the failure path: rolling
//! the local state-event cache back to its predecessor when no git server
//! or relay accepted the replacement. The caller builds, signs and caches
//! the planned state event before the transaction begins and keeps
//! ownership of user-facing reporting; [`StateTransactionFailure`] keeps
//! the failure reasons typed in between.
//!
//! `ngit sync` and `ngit init` still run their own divergent copies of
//! this flow; migrating them onto this module is planned follow-up work.

use std::collections::HashMap;

use anyhow::Result;
use console::Term;
use ngit::{
    client::{Client, delete_event_from_local_cache, save_event_in_local_cache, send_events},
    git::{Repo, RepoActions},
    push::push_to_remote,
    repo_ref::{RepoRef, format_grasp_server_url_as_relay_url, is_grasp_server_clone_url},
    utils::get_short_git_server_name,
};
use nostr::{Event, EventId, RelayUrl};

/// Side-effecting operations used by [`StateTransaction`]. Injected so the
/// transaction phases can be exercised deterministically in tests; the
/// production implementation is [`LiveOps`].
pub trait StateTransactionOps {
    /// Publish `events` to the given relays and report per-relay
    /// acceptance.
    async fn publish_events(
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

    /// Remove the event with `event_id` from the local nostr event cache.
    async fn delete_event_from_cache(&mut self, event_id: EventId) -> Result<()>;
}

/// [`StateTransactionOps`] implementation backed by the nostr client, the
/// local git repository's event cache and the real git servers.
pub struct LiveOps<'a> {
    pub client: &'a Client,
    pub git_repo: &'a Repo,
    pub repo_ref: &'a RepoRef,
    pub term: &'a Term,
    pub git_server_push_options: &'a [String],
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
            &self.repo_ref.to_nostr_git_url(&None),
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

    async fn delete_event_from_cache(&mut self, event_id: EventId) -> Result<()> {
        delete_event_from_local_cache(self.git_repo.get_path()?, event_id).await
    }
}

/// Outcome of executing the per-server git push plans.
pub enum GitStatePushOutcome {
    /// No git server remained eligible for a push (e.g. every paired GRASP
    /// relay rejected the staged state event).
    NoEligibleServers,
    /// Every eligible git server rejected the git data push.
    AllPushesFailed,
    /// At least one git server accepted every ref update pushed to it.
    AcceptedByGitServer,
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

/// A single repository-state publication transaction (see the module docs
/// for the phase ordering rationale).
///
/// The caller currently builds, signs and caches the planned state event
/// before the transaction begins, making the local cache authoritative
/// before any server has accepted the state. That premature caching is a
/// known issue slated for follow-up correctness work; the transaction
/// carries enough information to roll the cache mutation back if no git
/// server or relay accepts the replacement.
pub struct StateTransaction<'a> {
    repo_ref: &'a RepoRef,
    /// state events to publish (currently at most one)
    state_events: Vec<Event>,
    /// id of the planned state event already stored in the local cache
    new_state_event_id: Option<EventId>,
    /// the state event the local cache held before the planned one was
    /// stored; restored on rollback
    previous_state_event: Option<Event>,
    initial_state_publish: InitialStatePublish,
    remaining_relay_results: Vec<(String, bool)>,
}

impl<'a> StateTransaction<'a> {
    pub fn new(
        repo_ref: &'a RepoRef,
        state_events: Vec<Event>,
        new_state_event_id: Option<EventId>,
        previous_state_event: Option<Event>,
    ) -> Self {
        Self {
            repo_ref,
            state_events,
            new_state_event_id,
            previous_state_event,
            initial_state_publish: InitialStatePublish::default(),
            remaining_relay_results: vec![],
        }
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
        let grasp_relays = if self.state_events.is_empty() {
            vec![]
        } else {
            grasp_server_relay_urls(self.repo_ref)
        };

        let results = if self.state_events.is_empty() || grasp_relays.is_empty() {
            vec![]
        } else {
            match ops
                .publish_events(self.state_events.clone(), vec![], grasp_relays.clone())
                .await
            {
                Ok(results) => results,
                Err(error) => {
                    eprintln!("WARNING: failed to stage state event on grasp relays: {error:#}");
                    vec![]
                }
            }
        };

        self.initial_state_publish = InitialStatePublish {
            grasp_relays,
            results,
        };
    }

    /// Execute the per-server git push plans for the servers still
    /// eligible after GRASP staging.
    pub fn push_git_state_refspecs(
        &self,
        ops: &mut impl StateTransactionOps,
        remote_refspecs: HashMap<String, Vec<String>>,
        git_state_refspecs: &[String],
    ) -> GitStatePushOutcome {
        if git_state_refspecs.is_empty() {
            return GitStatePushOutcome::AcceptedByGitServer;
        }

        let servers_to_push = eligible_git_servers(
            remote_refspecs,
            git_state_refspecs,
            !self.state_events.is_empty(),
            &self.initial_state_publish.results,
        );

        if servers_to_push.is_empty() {
            return GitStatePushOutcome::NoEligibleServers;
        }

        let mut any_push_succeeded = false;
        for (git_server_url, server_refspecs) in &servers_to_push {
            if !server_refspecs.is_empty()
                && ops
                    .push_to_git_server(git_server_url, server_refspecs)
                    .is_ok_and(|ref_updates| all_ref_updates_accepted(&ref_updates))
            {
                any_push_succeeded = true;
            }
        }

        if any_push_succeeded {
            GitStatePushOutcome::AcceptedByGitServer
        } else {
            GitStatePushOutcome::AllPushesFailed
        }
    }

    /// Fan the state events out to the repo/user relays that were not
    /// already targeted during GRASP staging.
    pub async fn publish_state_to_remaining_relays(
        &mut self,
        ops: &mut impl StateTransactionOps,
        my_write_relays: &[String],
        repo_relay_only: bool,
    ) -> Result<()> {
        if self.state_events.is_empty() {
            return Ok(());
        }

        self.remaining_relay_results = publish_events_to_relays(
            ops,
            &self.repo_ref.relays,
            self.state_events.clone(),
            my_write_relays,
            repo_relay_only,
            Some(&self.initial_state_publish.grasp_relays),
        )
        .await?;
        Ok(())
    }

    /// Whether at least one relay accepted the state event across the
    /// initial GRASP staging and the remaining-relay fanout (vacuously
    /// true when the transaction carries no state event).
    pub fn state_relay_accepted(&self) -> bool {
        state_relay_accepted(
            !self.state_events.is_empty(),
            self.initial_state_publish.results.iter(),
            self.remaining_relay_results.iter(),
        )
    }

    /// Remove the newly-published state event from the local nostr cache
    /// and restore the previous state event (if any). This prevents a
    /// subsequent `ngit sync` or push from using a state that no git
    /// server ever accepted.
    pub async fn rollback(&self, ops: &mut impl StateTransactionOps) {
        let Some(new_state_event_id) = self.new_state_event_id else {
            return;
        };
        if let Err(e) = ops.delete_event_from_cache(new_state_event_id).await {
            eprintln!("WARNING: failed to roll back state event from local cache: {e}");
            return;
        }
        if let Some(old_event) = &self.previous_state_event {
            if let Err(e) = ops.save_event_in_cache(old_event).await {
                eprintln!("WARNING: failed to restore previous state event in local cache: {e}");
            }
        }
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
) -> Vec<(String, Vec<String>)> {
    let mut eligible = vec![];
    for (git_server_url, server_refspecs) in remote_refspecs {
        let server_refspecs = server_refspecs
            .iter()
            .filter(|refspec| git_state_refspecs.contains(refspec))
            .cloned()
            .collect::<Vec<String>>();
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
    eligible
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

fn grasp_server_relay_urls(repo_ref: &RepoRef) -> Vec<RelayUrl> {
    repo_ref
        .git_server
        .iter()
        .filter_map(|git_server_url| {
            if !is_grasp_server_clone_url(git_server_url) {
                return None;
            }
            format_grasp_server_url_as_relay_url(git_server_url)
                .ok()
                .and_then(|relay_url| RelayUrl::parse(&relay_url).ok())
        })
        .fold(Vec::new(), |mut relays, relay| {
            if !relays.iter().any(|existing| existing == &relay) {
                relays.push(relay);
            }
            relays
        })
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
        EventBuilder, Keys, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
        nips::nip19::ToBech32,
    };

    use super::*;

    fn state_event() -> Event {
        let keys = Keys::generate();
        keys.sign_event(
            EventBuilder::new(STATE_KIND, "")
                .tags([
                    Tag::identifier("test-repo"),
                    Tag::parse([
                        "refs/heads/main",
                        "1111111111111111111111111111111111111111",
                    ])
                    .unwrap(),
                ])
                .finalize_unsigned(keys.public_key()),
        )
        .unwrap()
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
        Publish {
            event_ids: Vec<EventId>,
            my_write_relays: Vec<String>,
            repo_relays: Vec<String>,
        },
        Push {
            git_server_url: String,
            refspecs: Vec<String>,
        },
        SaveToCache(EventId),
        DeleteFromCache(EventId),
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
        fail_cache_removal: bool,
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
                .filter(|call| matches!(call, FakeCall::Publish { .. }))
                .collect()
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

        async fn delete_event_from_cache(&mut self, event_id: EventId) -> Result<()> {
            self.calls.push(FakeCall::DeleteFromCache(event_id));
            if self.fail_cache_removal {
                anyhow::bail!("cache removal failed")
            }
            Ok(())
        }
    }

    mod publish_state_to_grasps_first {
        use super::*;

        #[tokio::test]
        async fn no_state_event_skips_grasp_staging() {
            let repo_ref = test_repo_ref(vec![grasp_clone_url("grasp.example")], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![], None, None);

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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn stages_state_on_grasp_relays_only() {
            let repo_ref = test_repo_ref(vec![grasp_clone_url("grasp.example")], vec![]);
            let event = state_event();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![event.clone()], None, None);

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert_eq!(
                ops.calls,
                vec![FakeCall::Publish {
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
            let event = state_event();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![event.clone()], None, None);

            transaction.publish_state_to_grasps_first(&mut ops).await;

            assert_eq!(
                ops.calls,
                vec![FakeCall::Publish {
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
        async fn server_with_no_matching_refspecs_counts_as_failed_without_pushing() {
            // A server whose plan doesn't intersect the state refspecs still
            // lands in the eligible set but is never pushed, so the outcome
            // is AllPushesFailed rather than NoEligibleServers. This pins
            // existing behavior; the follow-up correctness PR revisits
            // empty/no-op server plans.
            let vanilla_url = "https://vanilla.example/repo.git".to_string();
            let repo_ref = test_repo_ref(vec![vanilla_url.clone()], vec![]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(
                    vanilla_url,
                    vec!["refs/heads/other:refs/heads/other".to_string()],
                )]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AllPushesFailed));
            assert!(ops.pushed_servers().is_empty());
        }

        #[test]
        fn grasp_server_ineligible_when_staging_returned_no_results() {
            // Replaces the fail-open characterization
            // `grasp_server_eligible_when_staging_returned_no_results`: a
            // total staging failure must not let git data through to a
            // paired GRASP server whose relay never accepted the state.
            let grasp_url = grasp_clone_url("grasp.example");

            let servers = super::super::eligible_git_servers(
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
                true,
                &[],
            );

            assert!(servers.is_empty());
        }

        #[test]
        fn grasp_server_ineligible_when_paired_relay_result_is_missing() {
            let grasp_url = grasp_clone_url("grasp.example");

            let servers = super::super::eligible_git_servers(
                HashMap::from([(grasp_url, vec![refspec()])]),
                &[refspec()],
                true,
                // staging produced results, but none for this server's
                // paired relay
                &[("wss://other.example".to_string(), true)],
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
            transaction.publish_state_to_grasps_first(&mut ops).await;

            let outcome = transaction.push_git_state_refspecs(
                &mut ops,
                HashMap::from([(vanilla_a, vec![refspec()]), (vanilla_b, vec![refspec()])]),
                &[refspec()],
            );

            assert!(matches!(outcome, GitStatePushOutcome::AcceptedByGitServer));
        }
    }

    mod publish_state_to_remaining_relays {
        use super::*;

        #[tokio::test]
        async fn no_state_event_publishes_nothing() {
            let repo_ref = test_repo_ref(vec![], vec!["wss://relay.example"]);
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![], None, None);
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
            let event = state_event();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![event.clone()], None, None);
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
                    &FakeCall::Publish {
                        event_ids: vec![event.id],
                        my_write_relays: vec![],
                        repo_relays: vec!["wss://grasp.example".to_string()],
                    },
                    &FakeCall::Publish {
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
            let event = state_event();
            let mut ops = FakeOps::default();
            let mut transaction = StateTransaction::new(&repo_ref, vec![event.clone()], None, None);
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
                vec![FakeCall::Publish {
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
            let mut transaction = StateTransaction::new(&repo_ref, vec![state_event()], None, None);
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
            state_events: Vec<Event>,
            ops: &mut FakeOps,
            my_write_relays: &[String],
        ) -> bool {
            let mut transaction = StateTransaction::new(repo_ref, state_events, None, None);
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

            assert!(run_transaction(&repo_ref, vec![], &mut ops, &[]).await);
        }

        #[tokio::test]
        async fn not_accepted_when_state_event_present_but_no_relay_responded() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let mut ops = FakeOps::default();

            assert!(!run_transaction(&repo_ref, vec![state_event()], &mut ops, &[]).await);
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

            assert!(run_transaction(&repo_ref, vec![state_event()], &mut ops, &[]).await);
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

            assert!(run_transaction(&repo_ref, vec![state_event()], &mut ops, &[]).await);
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

            assert!(!run_transaction(&repo_ref, vec![state_event()], &mut ops, &[]).await);
        }
    }

    mod rollback {
        use super::*;

        #[tokio::test]
        async fn removes_planned_event_and_restores_previous() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let new_event = state_event();
            let previous = state_event();
            let mut ops = FakeOps::default();
            let transaction = StateTransaction::new(
                &repo_ref,
                vec![new_event.clone()],
                Some(new_event.id),
                Some(previous.clone()),
            );

            transaction.rollback(&mut ops).await;

            assert_eq!(
                ops.calls,
                vec![
                    FakeCall::DeleteFromCache(new_event.id),
                    FakeCall::SaveToCache(previous.id),
                ]
            );
        }

        #[tokio::test]
        async fn does_nothing_without_a_planned_event_id() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let mut ops = FakeOps::default();
            let transaction = StateTransaction::new(&repo_ref, vec![], None, Some(state_event()));

            transaction.rollback(&mut ops).await;

            assert!(ops.calls.is_empty());
        }

        #[tokio::test]
        async fn does_not_restore_previous_when_removal_fails() {
            let repo_ref = test_repo_ref(vec![], vec![]);
            let new_event = state_event();
            let mut ops = FakeOps {
                fail_cache_removal: true,
                ..Default::default()
            };
            let transaction = StateTransaction::new(
                &repo_ref,
                vec![new_event.clone()],
                Some(new_event.id),
                Some(state_event()),
            );

            transaction.rollback(&mut ops).await;

            assert_eq!(ops.calls, vec![FakeCall::DeleteFromCache(new_event.id)]);
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
