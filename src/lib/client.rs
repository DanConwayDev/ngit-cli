// have you considered

// TO USE ASYNC

// in traits (required for mocking unit tests)
// https://rust-lang.github.io/async-book/07_workarounds/05_async_in_traits.html
// https://github.com/dtolnay/async-trait
// see https://blog.rust-lang.org/inside-rust/2022/11/17/async-fn-in-trait-nightly.html
// I think we can use the async-trait crate and switch to the native feature
// which is currently in nightly. alternatively we can use nightly as it looks
// certain that the implementation is going to make it to stable but we don't
// want to inadvertlty use other features of nightly that might be removed.
use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, Write},
    fs::create_dir_all,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use console::Style;
use futures::{
    future::join_all,
    stream::{self, StreamExt},
};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
#[cfg(test)]
use mockall::*;
use nostr::prelude::{
    Event, EventBuilder, EventId, Filter, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp,
    event::UnsignedEvent,
    message::MachineReadablePrefix,
    nip01::Coordinate,
    nip05::{Nip05Address, Nip05Profile},
    nip19::Nip19Coordinate,
};
use nostr_database::{NostrDatabase, SaveEventStatus};
use nostr_lmdb::NostrLmdb;
use nostr_memory::MemoryDatabase;
use nostr_sdk::{
    client::ClientBuilder,
    error::{Error as NostrSdkError, ErrorKind as NostrSdkErrorKind},
    proxy::Proxy,
    relay::{RelayLimits, RelayNotification, RelayStatus},
};
use serde_json::Value;

use crate::{
    get_dirs,
    git::{Repo, RepoActions, get_git_config_item},
    git_events::{
        KIND_COMMENT, KIND_COVER_NOTE, KIND_LABEL, KIND_PRIVATE_GIT_RELAY_LIST, KIND_PULL_REQUEST,
        KIND_PULL_REQUEST_UPDATE, KIND_USER_GRASP_LIST, event_is_cover_letter,
        event_is_patch_set_root, event_is_revision_root, event_is_valid_pr_or_pr_update,
        status_kinds,
    },
    login::{
        get_likely_logged_in_user,
        user::{PrivateGitRelayDiscovery, get_user_ref_from_cache},
    },
    output_mode::{TransientLine, is_quiet, is_verbose, write_progress_line},
    relay_auth::{PolicyAuthenticator, RelayAuthMode, RelayAuthPolicy},
    repo_ref::{
        RepoRef, announcement_author_declines_maintainership,
        announcement_author_declines_moderatorship,
        announcement_author_has_only_malformed_self_records,
        announcement_author_validly_declines_maintainership,
        announcement_author_validly_declines_moderatorship, announcement_invalid_self_defers,
        normalize_grasp_server_url,
    },
    repo_state::RepoState,
    signer::NgitSigner,
    version_check,
};

/// Default SOCKS5 proxy used to reach `.onion` relays and clone URLs.
///
/// Override with `NGIT_TOR_PROXY=host:port`. Set to `none` (or empty) to
/// disable routing `.onion` traffic through a SOCKS5 proxy, in which case
/// `.onion` relays / clone URLs will be unreachable on hosts without their
/// own transparent Tor proxy.
pub const DEFAULT_TOR_SOCKS5_PROXY: &str = "127.0.0.1:9050";
pub const TOR_BROWSER_SOCKS5_PROXY: &str = "127.0.0.1:9150";

const TOR_PROXY_PROBE_TIMEOUT: Duration = Duration::from_millis(200);
static TOR_SOCKS5_PROXY: OnceLock<Option<std::net::SocketAddr>> = OnceLock::new();

/// Return a reachable SOCKS5 proxy for `.onion` traffic, if one is available.
///
/// Reads `NGIT_TOR_PROXY`:
///   - unset → probe [`DEFAULT_TOR_SOCKS5_PROXY`] and
///     [`TOR_BROWSER_SOCKS5_PROXY`]
///   - `""` / `none` / `off` / `disable` → `None`
///   - `host:port` → use it if a bounded TCP probe succeeds
pub fn tor_socks5_proxy_addr() -> Option<std::net::SocketAddr> {
    *TOR_SOCKS5_PROXY.get_or_init(|| {
        discover_tor_socks5_proxy(std::env::var("NGIT_TOR_PROXY").ok().as_deref(), |addr| {
            std::net::TcpStream::connect_timeout(&addr, TOR_PROXY_PROBE_TIMEOUT).is_ok()
        })
    })
}

fn discover_tor_socks5_proxy(
    configured: Option<&str>,
    mut is_reachable: impl FnMut(std::net::SocketAddr) -> bool,
) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let candidates: Vec<&str> = match configured.map(str::trim) {
        Some(value)
            if matches!(
                value.to_ascii_lowercase().as_str(),
                "" | "none" | "off" | "disable" | "disabled"
            ) =>
        {
            return None;
        }
        Some(value) => vec![value],
        None => vec![DEFAULT_TOR_SOCKS5_PROXY, TOR_BROWSER_SOCKS5_PROXY],
    };

    candidates
        .into_iter()
        .filter_map(|candidate| candidate.to_socket_addrs().ok())
        .flatten()
        .find(|addr| is_reachable(*addr))
}

pub fn ensure_onion_url_reachable(url: &str) -> Result<()> {
    if crate::git::nostr_url::host_is_onion(url) && tor_socks5_proxy_addr().is_none() {
        bail!(
            "cannot reach .onion address {url}: no Tor SOCKS5 proxy is available; start Tor on 127.0.0.1:9050 or 127.0.0.1:9150, or set NGIT_TOR_PROXY=host:port"
        );
    }
    Ok(())
}

/// Wire up nostr-sdk's per-relay SOCKS5 proxy so `.onion` relays go through
/// the configured Tor proxy and clearnet relays stay direct.
fn apply_onion_proxy(builder: ClientBuilder) -> ClientBuilder {
    match tor_socks5_proxy_addr() {
        Some(addr) => builder.proxy(Proxy::onion(addr)),
        None => builder,
    }
}

/// Build nostr-sdk with ngit's session-scoped NIP-42 policy. The client always
/// has the policy authenticator installed, but it cannot sign until the command
/// explicitly attaches a signer.
fn build_nostr_client(auth_policy: Arc<RelayAuthPolicy>) -> nostr_sdk::client::Client {
    crate::tls::install_default_crypto_provider();
    apply_onion_proxy(
        ClientBuilder::default()
            .relay_limits(RelayLimits::disable())
            .verify_subscriptions(true)
            .authenticator(PolicyAuthenticator::new(auth_policy)),
    )
    .build()
}

const SPINNER_EXPAND_DELAY_MS: u64 = 5000;
const RELAY_FETCH_HEADING: &str = "Checking nostr relays...";

static INVITED_MAINTAINER_WARNING_PRINTED: AtomicBool = AtomicBool::new(false);
static VERSION_CHECK_STATE_REQUESTED: AtomicBool = AtomicBool::new(false);
static GLOBAL_MEMORY_CACHE: OnceLock<Arc<dyn NostrDatabase>> = OnceLock::new();
static GLOBAL_CACHE_WARNING_PRINTED: AtomicBool = AtomicBool::new(false);

/// Holds the final state of a progress bar that finished before the detail
/// view was revealed. The style and prefix are already set on the bar; only
/// the `finish_with_message` call is deferred.
struct DeferredFinish {
    bar: ProgressBar,
    message: String,
}

/// Coordinates the transition from spinner to detail progress bars.
/// While `revealed` is false, `finish_bar` stores finish operations in
/// `deferred`. The background timer sets `revealed` to true, switches the
/// draw target, and flushes all deferred finishes so every bar appears.
struct BarRevealState {
    revealed: AtomicBool,
    deferred: Mutex<Vec<DeferredFinish>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayProgressMode {
    Concise,
    Detailed,
    Hidden,
}

/// Owns the complete lifecycle of one multi-relay progress report.
///
/// Normal commands start with one concise spinner and reveal per-relay rows
/// only after the shared delay. Partial failures remain transient once the
/// operation completes; an unavailable required relay set retains its
/// diagnostics. Verbose commands always retain details, while silent or test
/// commands never expose a renderer.
pub struct RelayProgressReporter {
    details: Option<MultiProgress>,
    mode: RelayProgressMode,
    _spinner_multi: Option<MultiProgress>,
    spinner: Option<ProgressBar>,
    heading: Option<ProgressBar>,
    heading_message: String,
    reveal_state: Option<Arc<BarRevealState>>,
    timer_handle: Option<tokio::task::JoinHandle<()>>,
    fetch_relay_health: FetchRelayHealth,
    finished: bool,
}

#[derive(Default)]
struct FetchRelayHealth {
    repository_relay_attempts: usize,
    repository_relay_successes: usize,
}

#[derive(Clone)]
pub struct RelayProgressHandle {
    details: MultiProgress,
    reveal_state: Option<Arc<BarRevealState>>,
}

impl RelayProgressHandle {
    fn add(&self, progress_bar: ProgressBar) -> ProgressBar {
        self.details.add(progress_bar)
    }

    fn finish_bar(&self, progress_bar: &ProgressBar, message: String) {
        finish_bar(progress_bar, message, &self.reveal_state);
    }
}

impl RelayProgressReporter {
    fn new(heading_message: impl Into<String>, animate: bool, silent: bool) -> Self {
        let heading_message = heading_message.into();
        let mode = if silent || is_quiet() || std::env::var("NGITTEST").is_ok() {
            RelayProgressMode::Hidden
        } else if is_verbose() || !animate {
            RelayProgressMode::Detailed
        } else {
            RelayProgressMode::Concise
        };
        Self::with_mode(heading_message, mode)
    }

    /// Construct the standard relay-fetch report used by non-silent commands.
    pub fn fetching() -> Self {
        Self::new(RELAY_FETCH_HEADING, true, false)
    }

    /// Construct a hidden reporter for alternative [`Connect`]
    /// implementations and queries which own no terminal UI.
    pub fn hidden() -> Self {
        Self::with_mode(String::new(), RelayProgressMode::Hidden)
    }

    fn with_mode(heading_message: String, mode: RelayProgressMode) -> Self {
        let (spinner_multi, spinner) = if mode == RelayProgressMode::Concise {
            let multi = MultiProgress::new();
            let spinner = multi.add(
                ProgressBar::new_spinner()
                    .with_style(
                        ProgressStyle::with_template("{spinner} {msg}")
                            .unwrap()
                            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈"),
                    )
                    .with_message(heading_message.clone()),
            );
            spinner.enable_steady_tick(Duration::from_millis(100));
            (Some(multi), Some(spinner))
        } else {
            (None, None)
        };

        let details = if mode == RelayProgressMode::Detailed {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };
        let heading = (mode != RelayProgressMode::Hidden).then(|| {
            details
                .add(ProgressBar::new(0).with_style(ProgressStyle::with_template("{msg}").unwrap()))
        });
        if mode == RelayProgressMode::Detailed {
            if let Some(heading) = &heading {
                heading.finish_with_message(heading_message.clone());
            }
        }

        let reveal_state = (mode == RelayProgressMode::Concise).then(|| {
            Arc::new(BarRevealState {
                revealed: AtomicBool::new(false),
                deferred: Mutex::new(Vec::new()),
            })
        });
        let timer_handle = if mode == RelayProgressMode::Concise {
            let details_for_timer = details.clone();
            let spinner_for_timer = spinner.clone();
            let heading_for_timer = heading.clone();
            let heading_message_for_timer = heading_message.clone();
            let reveal_state_for_timer = reveal_state.clone().unwrap();
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(SPINNER_EXPAND_DELAY_MS)).await;
                reveal_relay_progress(
                    &details_for_timer,
                    spinner_for_timer.as_ref(),
                    heading_for_timer.as_ref(),
                    &heading_message_for_timer,
                    &reveal_state_for_timer,
                );
            }))
        } else {
            None
        };

        Self {
            details: Some(details),
            mode,
            _spinner_multi: spinner_multi,
            spinner,
            heading,
            heading_message,
            reveal_state,
            timer_handle,
            fetch_relay_health: FetchRelayHealth::default(),
            finished: false,
        }
    }

    pub fn handle(&self) -> RelayProgressHandle {
        RelayProgressHandle {
            details: self.details.as_ref().unwrap().clone(),
            reveal_state: self.reveal_state.clone(),
        }
    }

    pub fn finish(
        mut self,
        has_errors: bool,
        show_concise_details: bool,
        concise_summary: Option<String>,
    ) -> Result<()> {
        if let Some(handle) = self.timer_handle.take() {
            handle.abort();
        }
        if self.mode == RelayProgressMode::Concise && show_concise_details {
            reveal_relay_progress(
                self.details.as_ref().unwrap(),
                self.spinner.as_ref(),
                self.heading.as_ref(),
                &self.heading_message,
                self.reveal_state.as_ref().unwrap(),
            );
        } else if let Some(spinner) = &self.spinner {
            spinner.finish_and_clear();
        }

        let retain_details = retain_relay_progress_details(self.mode, show_concise_details);
        let details = self.details.take().unwrap();
        if !retain_details {
            details.clear()?;
        }
        drop(details);

        if self.mode == RelayProgressMode::Concise {
            if let Some(summary) = concise_summary {
                console::Term::stderr().write_line(&summary)?;
            } else if show_concise_details && has_errors {
                console::Term::stderr().write_line("")?;
            }
        } else if self.mode == RelayProgressMode::Detailed && has_errors {
            console::Term::stderr().write_line("")?;
        }
        self.finished = true;
        Ok(())
    }
}

fn retain_relay_progress_details(mode: RelayProgressMode, show_concise_details: bool) -> bool {
    mode == RelayProgressMode::Detailed
        || (mode == RelayProgressMode::Concise && show_concise_details)
}

impl Drop for RelayProgressReporter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(handle) = self.timer_handle.take() {
            handle.abort();
        }
        if let Some(spinner) = &self.spinner {
            spinner.finish_and_clear();
        }
        if let Some(details) = &self.details {
            let _ = details.clear();
        }
    }
}

fn reveal_relay_progress(
    details: &MultiProgress,
    spinner: Option<&ProgressBar>,
    heading: Option<&ProgressBar>,
    heading_message: &str,
    reveal_state: &BarRevealState,
) {
    let mut deferred = reveal_state.deferred.lock().unwrap();
    if reveal_state.revealed.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }
    details.set_draw_target(ProgressDrawTarget::stderr());
    if let Some(heading) = heading {
        heading.finish_with_message(heading_message.to_owned());
    }
    for finish in deferred.drain(..) {
        finish.bar.finish_with_message(finish.message);
    }
}

/// Finish a progress bar, deferring the operation if the detail view has not
/// yet been revealed. When `reveal_state` is `None` (verbose or test mode),
/// the bar is finished immediately.
fn finish_bar(bar: &ProgressBar, message: String, reveal_state: &Option<Arc<BarRevealState>>) {
    match reveal_state {
        None => bar.finish_with_message(message),
        Some(state) => {
            // Lock the deferred list and check `revealed` while holding the
            // lock. The timer also holds this lock when it sets `revealed`
            // and drains the list, so there is no window where a bar could
            // be pushed after the drain.
            let mut deferred = state.deferred.lock().unwrap();
            if state.revealed.load(Ordering::Acquire) {
                drop(deferred);
                bar.finish_with_message(message);
            } else {
                // Style and prefix are already set on the bar. Store the
                // pending finish so the timer can apply it after reveal.
                deferred.push(DeferredFinish {
                    bar: bar.clone(),
                    message,
                });
            }
        }
    }
}

#[allow(clippy::struct_field_names)]
pub struct Client {
    client: nostr_sdk::client::Client,
    relay_default_set: Vec<String>,
    announcement_indexer_relays: Vec<String>,
    blaster_relays: Vec<String>,
    fallback_signer_relays: Vec<String>,
    grasp_default_set: Vec<String>,
    relays_not_to_retry: Arc<RwLock<HashMap<RelayUrl, String>>>,
    auth_policy: Arc<RelayAuthPolicy>,
}

impl Client {
    /// Marks a relay as skipped for the current session with a given reason.
    /// This method encapsulates the write lock for the relays_not_to_retry map.
    fn skip_relay_for_session(&self, relay_url: RelayUrl, reason: String) {
        self.relays_not_to_retry
            .write()
            .unwrap()
            .insert(relay_url, reason);
    }

    /// Checks if a relay should be skipped for the current session and returns
    /// the reason if it is. This method encapsulates the read lock for the
    /// relays_not_to_retry map.
    fn is_relay_skipped_for_session(&self, relay_url: &RelayUrl) -> Option<String> {
        self.relays_not_to_retry
            .read()
            .unwrap()
            .get(relay_url)
            .cloned()
    }
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Connect {
    fn default() -> Self;
    fn new(opts: Params) -> Self;
    async fn set_signer(&mut self, signer: Arc<NgitSigner>);
    async fn connect(&self, relay_url: &RelayUrl) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    fn get_relay_default_set(&self) -> &Vec<String>;
    fn get_announcement_indexer_relays(&self) -> &Vec<String>;
    fn get_blaster_relays(&self) -> &Vec<String>;
    fn get_fallback_signer_relays(&self) -> &Vec<String>;
    fn get_grasp_default_set(&self) -> &Vec<String>;
    /// Permit repository-relay authentication only with a signer already
    /// attached to this command.
    fn nip42_register_repo_relays(&self, relays: Vec<RelayUrl>);
    /// Require an explicitly acquired signer for known-private repository
    /// relays. This classification never acquires the signer itself.
    fn nip42_register_private_repo_relays(&self, relays: Vec<RelayUrl>);
    /// Permit authentication to the user's own relays for this publish.
    fn nip42_register_publish_relays(&self, relays: Vec<RelayUrl>);
    /// Attach a signer during account-creation publication, before a complete
    /// login object exists.
    fn nip42_set_auth_signer(&self, signer: Arc<NgitSigner>);
    async fn send_event_to<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        url: &str,
        event: nostr::event::Event,
    ) -> Result<nostr::prelude::EventId>;
    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::prelude::Filter>,
    ) -> Result<Vec<nostr::prelude::Event>>;
    async fn get_events_per_relay(
        &self,
        relays: Vec<RelayUrl>,
        filters: Vec<nostr::prelude::Filter>,
        progress: RelayProgressHandle,
    ) -> Result<Vec<Result<Vec<nostr::prelude::Event>>>>;
    async fn fetch_all<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        repo_coordinates: Option<&'a Nip19Coordinate>,
        user_profiles: &HashSet<PublicKey>,
        private_relay_list_authors: &HashSet<PublicKey>,
        repository_relays_only: bool,
    ) -> Result<(Vec<Result<FetchReport>>, RelayProgressReporter)>;
    async fn fetch_all_from_relay<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        request: FetchRequest,
        pb: &Option<ProgressBar>,
    ) -> Result<FetchReport>;
}

#[async_trait]
impl Connect for Client {
    fn default() -> Self {
        Self::new(Params::default())
    }

    fn new(opts: Params) -> Self {
        let auth_policy = Arc::new(RelayAuthPolicy::default());
        if let Some(keys) = opts.keys {
            auth_policy.set_signer(Arc::new(NgitSigner::Keys(keys)));
        }
        Client {
            client: build_nostr_client(Arc::clone(&auth_policy)),
            relay_default_set: opts.relay_default_set,
            announcement_indexer_relays: opts.announcement_indexer_relays,
            blaster_relays: opts.blaster_relays,
            fallback_signer_relays: opts.fallback_signer_relays,
            grasp_default_set: opts.grasp_default_set,
            relays_not_to_retry: Arc::new(RwLock::new(HashMap::new())),
            auth_policy,
        }
    }

    async fn set_signer(&mut self, signer: Arc<NgitSigner>) {
        self.auth_policy.set_signer(signer);
        // Preserve existing behavior: attaching a signer drops any anonymous
        // connections. Relay classifications survive through the shared policy.
        self.client = build_nostr_client(Arc::clone(&self.auth_policy));
    }

    async fn connect(&self, relay_url: &RelayUrl) -> Result<()> {
        ensure_onion_url_reachable(relay_url.as_str())?;
        if let Some(reason) = self.is_relay_skipped_for_session(relay_url) {
            bail!("{reason}");
        }
        self.client
            .add_relay(relay_url)
            .await
            .context("failed to add relay")?;

        let relay = self
            .client
            .relay(relay_url)
            .await?
            .ok_or_else(|| anyhow!("relay {} not found after add", relay_url))?;

        if !relay.status().is_connected() {
            #[allow(clippy::large_futures)]
            relay
                .try_connect()
                .timeout(std::time::Duration::from_secs(long_timeout()))
                .await?;
        }

        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await;
        Ok(())
    }

    fn get_relay_default_set(&self) -> &Vec<String> {
        &self.relay_default_set
    }

    fn get_announcement_indexer_relays(&self) -> &Vec<String> {
        &self.announcement_indexer_relays
    }

    fn get_blaster_relays(&self) -> &Vec<String> {
        &self.blaster_relays
    }

    fn get_fallback_signer_relays(&self) -> &Vec<String> {
        &self.fallback_signer_relays
    }

    fn get_grasp_default_set(&self) -> &Vec<String> {
        &self.grasp_default_set
    }

    fn nip42_register_repo_relays(&self, relays: Vec<RelayUrl>) {
        self.auth_policy.register_repo_relays(relays);
    }

    fn nip42_register_private_repo_relays(&self, relays: Vec<RelayUrl>) {
        self.auth_policy.register_private_repo_relays(relays);
    }

    fn nip42_register_publish_relays(&self, relays: Vec<RelayUrl>) {
        self.auth_policy.register_publish_relays(relays);
    }

    fn nip42_set_auth_signer(&self, signer: Arc<NgitSigner>) {
        self.auth_policy.set_signer(signer);
    }

    async fn send_event_to<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        url: &str,
        event: Event,
    ) -> Result<nostr::prelude::EventId> {
        ensure_onion_url_reachable(url)?;
        self.client.add_relay(url).await?;
        // A challenge declined before this relay became an authenticated
        // target is spent. Reconnect once so the relay can issue a fresh one.
        if let Ok(relay_url) = RelayUrl::parse(url) {
            if self.auth_policy.take_stale_declined(&relay_url) {
                if let Some(relay) = self.client.relay(&relay_url).await? {
                    relay.disconnect();
                }
            }
        }
        #[allow(clippy::large_futures)]
        self.client.connect_relay(url).await?;
        match self
            .client
            .relay(url)
            .await?
            .ok_or_else(|| anyhow!("relay not found: {url}"))?
            .send_event(&event)
            .await
        {
            Ok(_) => {}
            // the relay already holds this event, which is delivery
            // success, not failure
            Err(error) if event_rejection_is_duplicate(&error) => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(git_repo_path) = git_repo_path {
            save_event_in_local_cache(git_repo_path, &event).await?;
        }
        if [
            Kind::GitRepoAnnouncement,
            KIND_USER_GRASP_LIST,
            Kind::Metadata,
            Kind::RelayList,
        ]
        .contains(&event.kind)
        {
            save_event_in_global_cache(git_repo_path, &event).await?;
        }
        Ok(event.id)
    }

    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::prelude::Filter>,
    ) -> Result<Vec<nostr::prelude::Event>> {
        // relay lists can come from network events (e.g. kind 10002), so a
        // malformed entry must be skipped rather than panic
        let relay_urls = relays
            .iter()
            .filter_map(|relay| match RelayUrl::parse(relay) {
                Ok(url) => Some(url),
                Err(error) => {
                    eprintln!("warning: skipping invalid relay url {relay}: {error}");
                    None
                }
            })
            .collect::<Vec<RelayUrl>>();
        let progress_reporter = RelayProgressReporter::hidden();
        let relay_results = self
            .get_events_per_relay(relay_urls, filters, progress_reporter.handle())
            .await?;
        progress_reporter.finish(
            relay_results.iter().any(Result::is_err),
            !relay_results.is_empty() && relay_results.iter().all(Result::is_err),
            None,
        )?;
        // relay outages degrade to an empty result; callers that must not
        // mistake an outage for absent events consult their own caches or
        // use get_events_per_relay directly
        if !relay_results.is_empty() && relay_results.iter().all(Result::is_err) {
            eprintln!("warning: no relay responded while fetching events; continuing without them");
        }
        Ok(get_dedup_events(relay_results))
    }

    async fn get_events_per_relay(
        &self,
        relays: Vec<RelayUrl>,
        filters: Vec<nostr::prelude::Filter>,
        progress: RelayProgressHandle,
    ) -> Result<Vec<Result<Vec<nostr::prelude::Event>>>> {
        // add relays
        for relay in &relays {
            self.client
                .add_relay(relay.as_str())
                .await
                .context("failed to add relay")?;
        }

        let relays_map = self.client.relays().await;

        // Static timeout for get_events_per_relay (no adaptive timeout here)
        let static_timeout = Arc::new(AtomicU64::new(long_timeout()));

        let futures: Vec<_> = relays
            .clone()
            .iter()
            // don't look for events on blaster
            .filter(|r| !r.as_str().contains("nostr.mutinywallet.com"))
            .map(|r| (relays_map.get(r).unwrap(), filters.clone()))
            .map(|(relay, filters)| {
                let static_timeout_clone = static_timeout.clone();
                let progress = progress.clone();
                async move {
                    let pb = if std::env::var("NGITTEST").is_err() {
                        let pb = progress.add(
                            ProgressBar::new(1)
                                .with_prefix(format!("{: <11}{}", "connecting", relay.url()))
                                .with_style(pb_style(static_timeout_clone)?),
                        );
                        pb.enable_steady_tick(Duration::from_millis(300));
                        Some(pb)
                    } else {
                        None
                    };
                    fn style_progress_bar_with_error(
                        relay_url: &RelayUrl,
                        pb: &Option<ProgressBar>,
                        error: &anyhow::Error,
                    ) -> String {
                        let message = console::style(
                            error.to_string().replace("relay pool error:", "error:"),
                        )
                        .for_stderr()
                        .red()
                        .to_string();
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(false));
                            pb.set_prefix(format!("{: <11}{}", "error", relay_url));
                        }
                        message
                    }
                    if let Some(reason) = self.is_relay_skipped_for_session(relay.url()) {
                        let message =
                            style_progress_bar_with_error(relay.url(), &pb, &anyhow!("{reason}"));
                        if let Some(pb) = &pb {
                            progress.finish_bar(pb, message);
                        }
                        bail!("{reason}");
                    }
                    if let Err(error) = ensure_onion_url_reachable(relay.url().as_str()) {
                        let message = style_progress_bar_with_error(relay.url(), &pb, &error);
                        if let Some(pb) = &pb {
                            progress.finish_bar(pb, message);
                        }
                        return Err(error);
                    }
                    #[allow(clippy::large_futures)]
                    match get_events_of(relay, filters, &pb).await {
                        Err(error) => {
                            // Check error for timeout/connection issues and add to skip list
                            if error.to_string().contains("connection timeout") {
                                self.skip_relay_for_session(relay.url().clone(), error.to_string());
                            }
                            let message = style_progress_bar_with_error(relay.url(), &pb, &error);
                            if let Some(pb) = &pb {
                                progress.finish_bar(pb, message);
                            }
                            Err(error)
                        }
                        Ok(res) => {
                            if let Some(pb) = &pb {
                                pb.set_style(pb_after_style(true));
                                pb.set_prefix(format!(
                                    "{: <11}{}",
                                    format!("{} events", res.len()),
                                    relay.url()
                                ));
                                progress.finish_bar(pb, String::new());
                            }
                            Ok(res)
                        }
                    }
                }
            })
            .collect();

        let relay_results: Vec<Result<Vec<nostr::prelude::Event>>> =
            stream::iter(futures).buffer_unordered(15).collect().await;

        Ok(relay_results)
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_all<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        selected_maintainer_coordinate: Option<&'a Nip19Coordinate>,
        user_profiles: &HashSet<PublicKey>,
        private_relay_list_authors: &HashSet<PublicKey>,
        repository_relays_only: bool,
    ) -> Result<(Vec<Result<FetchReport>>, RelayProgressReporter)> {
        let relay_default_set = &self
            .relay_default_set
            .iter()
            .filter_map(|r| RelayUrl::parse(r).ok())
            .collect::<HashSet<RelayUrl>>();
        let announcement_indexer_relays = &self
            .announcement_indexer_relays
            .iter()
            .filter_map(|r| RelayUrl::parse(r).ok())
            .collect::<HashSet<RelayUrl>>();

        let mut request = create_relays_request(
            git_repo_path,
            selected_maintainer_coordinate,
            user_profiles,
            private_relay_list_authors,
            relay_default_set.clone(),
            announcement_indexer_relays.clone(),
            repository_relays_only,
        )
        .await?;

        let mut progress_reporter = RelayProgressReporter::fetching();
        let progress = progress_reporter.handle();

        let repository_success_count = Arc::new(AtomicU64::new(0));
        let announcement_resolved = Arc::new(AtomicBool::new(false));
        let announcement_resolution_coordinate = (!request.announcement_profile_authors.is_empty())
            .then(|| selected_maintainer_coordinate.cloned())
            .flatten();

        let mut processed_relay_scopes = HashSet::new();
        let mut repository_relay_attempts = 0;

        let mut relay_reports: Vec<Result<FetchReport>> = vec![];

        loop {
            match request.repo_auth_mode {
                RelayAuthMode::Never => {}
                RelayAuthMode::IfSignerAttached => self
                    .auth_policy
                    .register_repo_relays(request.repo_relays.iter().cloned()),
                RelayAuthMode::Required => self
                    .auth_policy
                    .register_private_repo_relays(request.repo_relays.iter().cloned()),
            }
            let mut scheduled_relay_scopes = HashSet::new();
            let relay_requests = request
                .repo_relays
                .union(&request.user_relays_for_profiles)
                .chain(request.announcement_indexer_relays.iter())
                .filter(|&r| !r.as_str().contains("nostr.mutinywallet.com"))
                .filter_map(|relay| {
                    let scoped = request.scoped_to_relay(relay);
                    let key = scoped.relay_processing_key()?;
                    (!processed_relay_scopes.contains(&key) && scheduled_relay_scopes.insert(key))
                        .then_some(scoped)
                })
                .collect::<Vec<_>>();
            if relay_requests.is_empty() {
                break;
            }
            repository_relay_attempts += relay_requests
                .iter()
                .filter(|request| request.scope == RelayFetchScope::Repository)
                .count();
            for relay in &request.repo_relays {
                self.client
                    .add_relay(relay.as_str())
                    .await
                    .context("failed to add relay")?;
            }

            let round_progress = Arc::new(FetchRoundProgress::new(&relay_requests));
            let repository_success_count_for_loop = repository_success_count.clone();
            let announcement_resolved_for_loop = announcement_resolved.clone();
            let processed_this_round = relay_requests
                .iter()
                .filter_map(FetchRequest::relay_processing_key)
                .collect::<Vec<_>>();

            let futures: Vec<_> = relay_requests
                .into_iter()
                .map(|request| {
                    let peers = round_progress.clone();
                    let scope = request.scope;
                    let repository_success_count_clone =
                        repository_success_count_for_loop.clone();
                    let current_timeout_clone = Arc::new(AtomicU64::new(long_timeout()));
                    let announcement_resolved_clone = announcement_resolved_for_loop.clone();
                    let announcement_resolution_coordinate =
                        announcement_resolution_coordinate.clone();
                    let progress = progress.clone();
                    let is_repository_relay = request.scope == RelayFetchScope::Repository;
                    let is_author_announcement_relay = request
                        .selected_relay
                        .as_ref()
                        .is_some_and(|relay| request.author_announcement_relays.contains(relay));
                    async move {
                        let relay_column_width = request.relay_column_width;

                        let relay_url = request
                            .selected_relay
                            .clone()
                            .context("fetch_all_from_relay called without a relay")?;
                        ensure_onion_url_reachable(relay_url.as_str())?;

                        // Always create a real progress bar added to the detail
                        // multi. In test mode the multi has a hidden draw target
                        // so nothing is displayed. In concise mode the multi
                        // starts hidden and the background timer reveals it.
                        let pb = progress.add(
                            ProgressBar::new(1)
                                .with_prefix(
                                    format!(
                                        "{: <relay_column_width$} connecting",
                                        relay_url
                                    )
                                    .to_string(),
                                )
                                .with_style(pb_style(current_timeout_clone.clone())?),
                        );
                        pb.enable_steady_tick(Duration::from_millis(300));
                        let pb = Some(pb);

                        /// Set error styling on a progress bar without finishing
                        /// it. Returns the error message so the caller can
                        /// finish the bar through the deferred mechanism.
                        fn style_progress_bar_with_error(
                            relay_column_width: usize,
                            relay_url: &RelayUrl,
                            pb: &Option<ProgressBar>,
                            error: &anyhow::Error,
                        ) -> String {
                            let msg = console::style(
                                error.to_string().replace("relay pool error:", "error:"),
                            )
                            .for_stderr()
                            .red()
                            .to_string();
                            if let Some(pb) = pb {
                                pb.set_style(pb_after_style(false));
                                pb.set_prefix(
                                    Style::new()
                                        .color256(247)
                                        .apply_to(format!("{: <relay_column_width$}", relay_url))
                                        .to_string(),
                                );
                            }
                            msg
                        }

                        if let Some(reason) = self.is_relay_skipped_for_session(&relay_url) {
                            let msg = style_progress_bar_with_error(
                                relay_column_width,
                                &relay_url,
                                &pb,
                                &anyhow!("{reason}"),
                            );
                            if let Some(ref bar) = pb {
                                progress.finish_bar(bar, msg);
                            }
                            bail!("{reason}");
                        }

                        let pb_clone = pb.clone();
                        let fetch_future = self.fetch_all_from_relay(git_repo_path, request, &pb_clone);
                        tokio::pin!(fetch_future);

                        let mut deadline = FetchDeadline::new(
                            Duration::from_secs(long_timeout()),
                            Duration::from_secs(short_timeout()),
                        );
                        let timeout_future = async {
                            loop {
                                let now = tokio::time::Instant::now();
                                let may_shorten = peers.has_quorum(scope)
                                    && !author_announcement_discovery_pending(
                                        is_author_announcement_relay,
                                        &announcement_resolved_clone,
                                    );
                                deadline.update(now, may_shorten);
                                current_timeout_clone.store(
                                    deadline.budget().as_secs(), Ordering::Relaxed,
                                );
                                if now >= deadline.end {
                                    return now.duration_since(deadline.start);
                                }
                                tokio::time::sleep_until(
                                    deadline.end.min(now + Duration::from_millis(100)),
                                ).await;
                            }
                        };

                        #[allow(clippy::large_futures)]
                        let result = tokio::select! {
                            result = &mut fetch_future => {
                                if !announcement_resolved_clone.load(Ordering::Acquire) {
                                    if let Some(coordinate) =
                                        &announcement_resolution_coordinate
                                    {
                                        if get_repo_ref_from_cache(git_repo_path, coordinate)
                                            .await
                                            .is_ok()
                                        {
                                            announcement_resolved_clone
                                                .store(true, Ordering::Release);
                                        }
                                    }
                                }
                                if result.is_ok() {
                                    peers.record_success(scope);
                                    if is_repository_relay {
                                        repository_success_count_clone.fetch_add(1, Ordering::Relaxed);
                                    }

                                }
                                result
                            }
                            elapsed = timeout_future => {
                                Err(anyhow!("timeout after {:.1}s", elapsed.as_secs_f64()))
                            }
                        };

                        match result {
                            Err(error) => {
                                if error.to_string().contains("connection timeout")
                                    || error.to_string().contains("timeout after")
                                {
                                    self.skip_relay_for_session(relay_url.clone(), error.to_string());
                                }
                                let msg = style_progress_bar_with_error(
                                    relay_column_width,
                                    &relay_url,
                                    &pb,
                                    &error,
                                );
                                if let Some(ref bar) = pb {
                                    progress.finish_bar(bar, msg);
                                }
                                Err(error)
                            }
                            Ok(res) => {
                                // The bar's style and prefix were already set
                                // by fetch_all_from_relay; finish it through
                                // the deferred mechanism.
                                if let Some(ref bar) = pb {
                                    progress.finish_bar(bar, String::new());
                                }
                                Ok(res)
                            }
                        }
                    }
                })
                .collect();

            for report in stream::iter(futures)
                .buffer_unordered(15)
                .collect::<Vec<Result<FetchReport>>>()
                .await
            {
                relay_reports.push(report);
            }
            processed_relay_scopes.extend(processed_this_round);

            let selected_repo_ref =
                if let Some(selected_maintainer_coordinate) = selected_maintainer_coordinate {
                    get_repo_ref_from_cache(git_repo_path, selected_maintainer_coordinate)
                        .await
                        .ok()
                } else {
                    None
                };
            let selected_announcement_resolved = selected_repo_ref.is_some();
            if !request.lock_repository_relays {
                if let Some(repo_ref) = &selected_repo_ref {
                    if repo_ref.private {
                        request.repo_auth_mode = RelayAuthMode::Required;
                    }
                    request.repo_relays = repo_ref.relays.iter().cloned().collect();
                }
            }
            if selected_announcement_resolved {
                request.complete_hintless_announcement_discovery();
            }

            request.user_relays_for_profiles = if request.repository_relays_only {
                HashSet::new()
            } else {
                let mut set = HashSet::new();
                for user in &request
                    .profiles_to_fetch_from_user_relays
                    .clone()
                    .into_keys()
                    .collect::<Vec<PublicKey>>()
                {
                    if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, user).await {
                        for r in user_ref.relays.write() {
                            if let Ok(url) = RelayUrl::parse(&r) {
                                set.insert(url);
                            }
                        }
                    }
                }
                set
            };
            if !selected_announcement_resolved && !request.repository_relays_only {
                if let Some(author) = request.announcement_profile_authors.iter().next().copied() {
                    if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, &author).await {
                        // A relay list learned in the preceding round becomes
                        // an announcement route in the next round.
                        request.add_author_announcement_relays(
                            user_ref
                                .relays
                                .write()
                                .into_iter()
                                .filter_map(|relay| RelayUrl::parse(&relay).ok()),
                        );
                    }
                }
            }
        }

        progress_reporter.fetch_relay_health = FetchRelayHealth {
            repository_relay_attempts,
            repository_relay_successes: repository_success_count.load(Ordering::Relaxed) as usize,
        };
        Ok((relay_reports, progress_reporter))
    }

    async fn fetch_all_from_relay<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        request: FetchRequest,
        pb: &Option<ProgressBar>,
    ) -> Result<FetchReport> {
        let mut fresh_coordinates: HashSet<Nip19Coordinate> = HashSet::new();
        for (c, _) in request.repo_coordinates_without_relays.clone() {
            fresh_coordinates.insert(c);
        }
        let mut fresh_proposal_roots = request.proposals.clone();
        let mut fresh_issue_roots = request.issue_ids.clone();
        let mut fresh_profiles: HashSet<PublicKey> = request
            .missing_contributor_profiles
            .union(
                &request
                    .profiles_to_fetch_from_user_relays
                    .clone()
                    .into_keys()
                    .collect(),
            )
            .copied()
            .collect();
        // Only request non-proposal event deletions on the first loop iteration;
        // cleared after first use so subsequent iterations don't re-request them.
        let mut fresh_non_proposal_event_ids = request.non_proposal_event_ids.clone();

        // discovery expansion state: see expand_role_discovery
        let mut maintainer_listed: HashSet<PublicKey> = request.maintainer_listed_authors.clone();
        let mut session_announcements: Vec<AnnouncementListing> = Vec::new();

        let mut report = FetchReport::default();

        let relay_url = request
            .selected_relay
            .clone()
            .context("fetch_all_from_relay called without a relay")?;
        ensure_onion_url_reachable(relay_url.as_str())?;

        // A challenge declined while this URL was only an indexer is spent.
        // Once an announcement promotes it to a repository relay, reconnect so
        // it can issue a fresh challenge under the upgraded policy.
        if self.auth_policy.take_stale_declined(&relay_url) {
            if let Some(relay) = self.client.relay(&relay_url).await? {
                relay.disconnect();
            }
        }

        let relay_column_width = request.relay_column_width;

        let _ = self.client.add_relay(&relay_url).await;

        let dim = Style::new().color256(247);

        loop {
            let mut filters = match request.scope {
                RelayFetchScope::Repository => get_fetch_filters(
                    &fresh_coordinates,
                    &fresh_proposal_roots,
                    &fresh_issue_roots,
                    &fresh_non_proposal_event_ids,
                    &fresh_profiles,
                ),
                RelayFetchScope::Auxiliary {
                    announcements,
                    profiles,
                } => get_auxiliary_fetch_filters(
                    &fresh_coordinates,
                    &fresh_profiles,
                    announcements,
                    profiles,
                ),
            };
            request.add_private_relay_list_filter(&mut filters);
            if version_check::is_version_check_relay(&relay_url)
                && !VERSION_CHECK_STATE_REQUESTED.swap(true, Ordering::AcqRel)
            {
                if let Ok(update_filters) =
                    version_check::background_update_filters_from_cache(git_repo_path).await
                {
                    filters.extend(update_filters);
                }
            }
            fresh_non_proposal_event_ids = HashSet::new();

            if let Some(pb) = &pb {
                pb.set_prefix(
                    dim.apply_to(format!(
                        "{: <relay_column_width$} {}",
                        relay_url,
                        if report.to_string().is_empty() {
                            "fetching".to_string()
                        } else {
                            format!("fetching... updates: {report}")
                        },
                    ))
                    .for_stderr()
                    .to_string(),
                );
            }

            fresh_coordinates = HashSet::new();
            fresh_proposal_roots = HashSet::new();
            fresh_issue_roots = HashSet::new();
            fresh_profiles = HashSet::new();

            let relay = self
                .client
                .relay(&relay_url)
                .await?
                .ok_or_else(|| anyhow!("relay not found: {relay_url}"))?;
            let events: Vec<nostr::prelude::Event> =
                get_events_of(&relay, filters.clone(), pb).await?;
            // TODO: try reconcile

            // Track the best state event seen from this relay so callers can
            // determine which relays have a stale or absent state event.
            // We must do this before process_fetched_events because the local
            // database only stores the canonical latest event; per-relay
            // visibility is only available here.
            for event in &events {
                if event.kind.eq(&STATE_KIND) {
                    let entry = report
                        .state_per_relay
                        .entry(relay_url.clone())
                        .or_insert(None);
                    let is_newer = entry
                        .as_ref()
                        .is_none_or(|existing: &nostr::prelude::Event| {
                            event.created_at.gt(&existing.created_at)
                                || (event.created_at.eq(&existing.created_at)
                                    && event.id.gt(&existing.id))
                        });
                    if is_newer {
                        *entry = Some(event.clone());
                    }
                }
            }
            // Mark relay as queried even if no state event was returned.
            report
                .state_per_relay
                .entry(relay_url.clone())
                .or_insert(None);

            process_fetched_events(
                events,
                &request,
                git_repo_path,
                &mut fresh_coordinates,
                &mut fresh_proposal_roots,
                &mut fresh_issue_roots,
                &mut fresh_profiles,
                &mut maintainer_listed,
                &mut session_announcements,
                &mut report,
            )
            .await?;

            let exhausted = match request.scope {
                RelayFetchScope::Repository => {
                    fresh_coordinates.is_empty()
                        && fresh_proposal_roots.is_empty()
                        && fresh_issue_roots.is_empty()
                        && fresh_profiles.is_empty()
                }
                RelayFetchScope::Auxiliary {
                    announcements,
                    profiles,
                } => {
                    (!announcements || fresh_coordinates.is_empty())
                        && (!profiles || fresh_profiles.is_empty())
                }
            };
            if exhausted {
                break;
            }
        }
        if let Some(pb) = pb {
            pb.set_style(pb_after_style(true));
            pb.set_prefix(format!(
                "{} {}",
                dim.apply_to(format!("{: <relay_column_width$}", relay_url))
                    .for_stderr(),
                if report.to_string().is_empty() {
                    "no new events".to_string()
                } else {
                    format!("new events: {report}")
                },
            ));
            // Don't call finish_with_message here — the caller handles
            // finishing through the deferred mechanism so bars that complete
            // before the detail view is revealed still appear correctly.
        }
        Ok(report)
    }
}

/// Repository history requires repository responses. Auxiliary work keeps the
/// overall threshold so an isolated metadata request cannot hold up a round.
struct FetchRoundProgress {
    repository: FetchScopeProgress,
    overall: FetchScopeProgress,
}

struct FetchScopeProgress {
    total: u64,
    successes: AtomicU64,
}

impl FetchScopeProgress {
    fn has_quorum(&self) -> bool {
        self.total > 0 && self.successes.load(Ordering::Relaxed) >= self.total.div_ceil(2)
    }
}

impl FetchRoundProgress {
    fn new(requests: &[FetchRequest]) -> Self {
        Self {
            repository: FetchScopeProgress {
                total: requests
                    .iter()
                    .filter(|r| r.scope == RelayFetchScope::Repository)
                    .count() as u64,
                successes: AtomicU64::new(0),
            },
            overall: FetchScopeProgress {
                total: requests.len() as u64,
                successes: AtomicU64::new(0),
            },
        }
    }

    fn record_success(&self, scope: RelayFetchScope) {
        if scope == RelayFetchScope::Repository {
            self.repository.successes.fetch_add(1, Ordering::Relaxed);
        }
        self.overall.successes.fetch_add(1, Ordering::Relaxed);
    }

    fn has_quorum(&self, scope: RelayFetchScope) -> bool {
        match scope {
            RelayFetchScope::Repository => self.repository.has_quorum(),
            RelayFetchScope::Auxiliary { .. } => self.overall.has_quorum(),
        }
    }
}

struct FetchDeadline {
    start: tokio::time::Instant,
    end: tokio::time::Instant,
    grace: Duration,
}

impl FetchDeadline {
    fn new(maximum: Duration, grace: Duration) -> Self {
        let start = tokio::time::Instant::now();
        Self {
            start,
            end: start + maximum,
            grace,
        }
    }

    fn update(&mut self, now: tokio::time::Instant, may_shorten: bool) {
        if may_shorten {
            // A late quorum must not extend the original absolute deadline.
            self.end = self.end.min(now + self.grace);
        }
    }

    fn budget(&self) -> Duration {
        self.end.duration_since(self.start)
    }
}

fn author_announcement_discovery_pending(
    is_author_announcement_relay: bool,
    announcement_resolved: &AtomicBool,
) -> bool {
    is_author_announcement_relay && !announcement_resolved.load(Ordering::Acquire)
}

fn long_timeout() -> u64 {
    if std::env::var("NGITTEST").is_ok() {
        1
    } else {
        45
    }
}

fn short_timeout() -> u64 {
    if std::env::var("NGITTEST").is_ok() {
        1
    } else {
        7
    }
}

async fn get_events_of(
    relay: &nostr_sdk::relay::Relay,
    filters: Vec<nostr::prelude::Filter>,
    pb: &Option<ProgressBar>,
) -> Result<Vec<Event>> {
    // relay.reconcile(filter, opts).await?;

    let mut retry_delay = Duration::from_secs(2);
    let start_time = std::time::Instant::now();
    let max_timeout = Duration::from_secs(long_timeout());
    let mut last_error = None;
    let mut attempt_num = 0;
    let dim = Style::new().color256(247);

    if let Some(pb) = pb {
        pb.set_prefix(
            console::style(relay.url())
                .for_stderr()
                .yellow()
                .to_string(),
        );
        pb.set_message("connecting");
    }
    while !relay.status().is_connected() {
        attempt_num += 1;
        #[allow(clippy::large_futures)]
        match relay
            .try_connect()
            .timeout(Duration::from_secs(short_timeout()))
            .await
        {
            Ok(_) => {
                if relay.status().is_connected() {
                    break;
                }
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
        // Check if we have time for another retry
        if start_time.elapsed() + retry_delay >= max_timeout {
            break;
        }

        // For short delays (< 2s), just show a simple message and sleep
        // For longer delays, show a countdown to provide feedback
        if retry_delay < Duration::from_secs(2) {
            if let Some(pb) = pb {
                let retry_msg = if attempt_num > 1 {
                    format!("retrying (attempt {attempt_num})")
                } else {
                    "retrying".to_string()
                };
                pb.set_message(format!(
                    "{} {}",
                    console::style("connection failed").for_stderr().red(),
                    dim.apply_to(retry_msg).for_stderr()
                ));
            }
            tokio::time::sleep(retry_delay).await;
        } else {
            // Countdown with dynamic updates for longer delays
            let retry_start = std::time::Instant::now();
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.tick().await; // First tick completes immediately

            loop {
                let elapsed = retry_start.elapsed();
                let remaining = retry_delay.saturating_sub(elapsed);

                if let Some(pb) = pb {
                    let retry_msg = if attempt_num > 1 {
                        format!(
                            "retrying in {:.0}s (attempt {attempt_num})",
                            remaining.as_secs_f64()
                        )
                    } else {
                        format!("retrying in {:.0}s", remaining.as_secs_f64())
                    };
                    pb.set_message(format!(
                        "{} {}",
                        console::style("connection failed").for_stderr().red(),
                        dim.apply_to(retry_msg).for_stderr()
                    ));
                }

                if elapsed >= retry_delay {
                    break;
                }

                interval.tick().await;
            }
        }

        // Check again after sleep
        if start_time.elapsed() >= max_timeout {
            break;
        }

        retry_delay = Duration::from_secs_f64(retry_delay.as_secs_f64() * 1.5);
    }

    if !relay.status().is_connected() {
        if let Some(e) = last_error {
            bail!("connection timeout: {}", e);
        } else {
            bail!("connection timeout here");
        }
    } else if let Some(pb) = pb {
        pb.set_prefix(
            console::style(relay.url())
                .for_stderr()
                .yellow()
                .to_string(),
        );
        pb.set_message("connected");
    }

    if filters.is_empty() {
        return Ok(Vec::new());
    }

    fetch_complete_events(relay, filters, Duration::from_secs(long_timeout())).await
}

async fn fetch_complete_events(
    relay: &nostr_sdk::relay::Relay,
    filters: Vec<Filter>,
    timeout: Duration,
) -> Result<Vec<Event>> {
    use nostr::prelude::{RelayMessage, SubscriptionId};

    // The SDK stream can end successfully on timeout or disconnection. Only
    // this subscription's EOSE proves that the relay completed its history.
    // Keep SDK validation and AUTH retry handling on the event stream itself.
    let id = SubscriptionId::generate();
    let mut notifications = relay.notifications();
    tokio::time::timeout(timeout, async {
        let mut stream = relay.stream_events(filters).with_id(id.clone()).await?;
        #[allow(clippy::mutable_key_type)]
        let mut events = HashSet::new();
        let mut received_eose = false;
        let mut drained = false;
        while !received_eose || !drained {
            tokio::select! {
                event = stream.next(), if !drained => {
                    match event {
                        Some(Ok(event)) => {
                            // Preserve the SDK fetch API's default buffer bound.
                            if events.len() >= 10_000 && !events.contains(&event) {
                                bail!("too many fetched events");
                            }
                            events.insert(event);
                        }
                        Some(Err(error)) => return Err(error.into()),
                        None => drained = true,
                    }
                }
                notification = notifications.next(), if !received_eose => {
                    match notification {
                        Some(RelayNotification::Message { message }) => {
                            if matches!(*message, RelayMessage::EndOfStoredEvents(ref subscription) if subscription.as_ref() == &id) {
                                received_eose = true;
                            }
                        }
                        Some(RelayNotification::RelayStatus {
                            status: RelayStatus::Disconnected | RelayStatus::Terminated | RelayStatus::Banned,
                        }) | None => bail!("relay disconnected before EOSE"),
                        _ => {}
                    }
                }
            }
        }
        Ok(events.into_iter().collect())
    }).await.context("timed out before complete EOSE")?
}

pub struct Params {
    pub keys: Option<nostr::prelude::Keys>,
    pub relay_default_set: Vec<String>,
    pub announcement_indexer_relays: Vec<String>,
    pub blaster_relays: Vec<String>,
    pub fallback_signer_relays: Vec<String>,
    pub grasp_default_set: Vec<String>,
}

/// Parse a `;`-separated string list from an env var.
///
/// Returns `Some(vec)` only when the env var is set and parses into a non-empty
/// list. Empty strings are silently filtered out; if every entry is filtered,
/// we return `None` so the caller falls back to the next config source.
fn env_list(var: &str) -> Option<Vec<String>> {
    let raw = std::env::var(var).ok()?;
    let parsed: Vec<String> = raw
        .split(';')
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })
        .collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

fn env_relay_url_list(var: &str) -> Option<Vec<String>> {
    env_list(var).and_then(|urls| {
        let parsed: Vec<String> = urls
            .iter()
            .filter_map(|url| RelayUrl::parse(url).ok())
            .map(|url| url.to_string())
            .collect();
        if parsed.is_empty() {
            None
        } else {
            Some(parsed)
        }
    })
}

fn env_grasp_server_list(var: &str) -> Option<Vec<String>> {
    env_list(var).and_then(|urls| {
        let parsed: Vec<String> = urls
            .iter()
            .filter_map(|url| normalize_grasp_server_url(url).ok())
            .collect();
        if parsed.is_empty() {
            None
        } else {
            Some(parsed)
        }
    })
}

impl Default for Params {
    fn default() -> Self {
        Params {
            keys: None,
            relay_default_set: if std::env::var("NGITTEST").is_ok() {
                env_relay_url_list("NGIT_RELAY_DEFAULT_SET").unwrap_or_else(|| {
                    vec![
                        "ws://localhost:8051".to_string(),
                        "ws://localhost:8052".to_string(),
                    ]
                })
            } else {
                vec![
                    "wss://relay.damus.io".to_string(), /* free, good reliability, have been
                                                         * known
                                                         * to delete all messages */
                    "wss://relay.ditto.pub".to_string(),
                    // "wss://nos.lol".to_string(), // always prompts for nip42 auth even for
                    // reading
                ]
            },
            announcement_indexer_relays: if std::env::var("NGITTEST").is_ok() {
                env_relay_url_list("NGIT_RELAY_ANNOUNCEMENT_INDEXER_SET").unwrap_or_default()
            } else {
                vec!["wss://index.ngit.dev".to_string()]
            },
            blaster_relays: if std::env::var("NGITTEST").is_ok() {
                env_relay_url_list("NGIT_RELAY_BLASTER_SET")
                    .unwrap_or_else(|| vec!["ws://localhost:8057".to_string()])
            } else {
                vec![]
            },
            fallback_signer_relays: if std::env::var("NGITTEST").is_ok() {
                env_relay_url_list("NGIT_RELAY_SIGNER_FALLBACK_SET")
                    .unwrap_or_else(|| vec!["ws://localhost:8051".to_string()])
            } else {
                vec![
                    "wss://bucket.coracle.social".to_string(),
                    "wss://nos.lol".to_string(),
                    "wss://relay.ditto.pub".to_string(),
                ]
            },
            grasp_default_set: if std::env::var("NGITTEST").is_ok() {
                env_grasp_server_list("NGIT_GRASP_DEFAULT_SET").unwrap_or_default()
            } else {
                vec!["relay.ngit.dev".to_string(), "gitnostr.com".to_string()]
            },
        }
    }
}
impl Params {
    fn apply_env_overrides(&mut self) {
        if let Some(relays) = env_relay_url_list("NGIT_RELAY_DEFAULT_SET") {
            self.relay_default_set = relays;
        }
        if let Some(relays) = env_relay_url_list("NGIT_RELAY_ANNOUNCEMENT_INDEXER_SET") {
            self.announcement_indexer_relays = relays;
        }
        if let Some(relays) = env_relay_url_list("NGIT_RELAY_BLASTER_SET") {
            self.blaster_relays = relays;
        }
        if let Some(relays) = env_relay_url_list("NGIT_RELAY_SIGNER_FALLBACK_SET") {
            self.fallback_signer_relays = relays;
        }
        if let Some(servers) = env_grasp_server_list("NGIT_GRASP_DEFAULT_SET") {
            self.grasp_default_set = servers;
        }
    }

    pub fn with_git_config_relay_defaults(git_repo: &Option<&Repo>) -> Self {
        let mut params = Params::default();
        if std::env::var("NGITTEST").is_err() {
            // ignore git config settings under test
            if let Ok(Some(relay_defaults)) =
                get_git_config_item(git_repo, "nostr.relay-default-set")
            {
                let new_default_relays: Vec<String> = relay_defaults
                    .split(';')
                    .filter_map(|url| RelayUrl::parse(url).ok()) // Attempt to parse and filter out errors
                    .map(|relay_url| relay_url.to_string()) // Convert RelayUrl back to String
                    .collect();
                // elsewhere it is assumed this isn't empty
                if !new_default_relays.is_empty() {
                    params.relay_default_set = new_default_relays;
                }
            }
            if let Ok(Some(relay_blasters)) =
                get_git_config_item(git_repo, "nostr.relay-blaster-set")
            {
                params.blaster_relays = relay_blasters
                    .split(';')
                    .filter_map(|url| RelayUrl::parse(url).ok()) // Attempt to parse and filter out errors
                    .map(|relay_url| relay_url.to_string()) // Convert RelayUrl back to String
                    .collect();
            }
            if let Ok(Some(announcement_indexers)) =
                get_git_config_item(git_repo, "nostr.relay-announcement-indexer-set")
            {
                params.announcement_indexer_relays = announcement_indexers
                    .split(';')
                    .filter_map(|url| RelayUrl::parse(url).ok())
                    .map(|relay_url| relay_url.to_string())
                    .collect();
            }
            if let Ok(Some(relay_signer)) =
                get_git_config_item(git_repo, "nostr.relay-signer-fallback-set")
            {
                params.fallback_signer_relays = relay_signer
                    .split(';')
                    .filter_map(|url| RelayUrl::parse(url).ok()) // Attempt to parse and filter out errors
                    .map(|relay_url| relay_url.to_string()) // Convert RelayUrl back to String
                    .collect();
            }
            if let Ok(Some(grasp_default_servers)) =
                get_git_config_item(git_repo, "nostr.grasp-default-set")
            {
                let new_default_grasp_servers: Vec<String> = grasp_default_servers
                    .split(';')
                    .filter_map(|url| normalize_grasp_server_url(url).ok()) // Attempt to parse and filter out errors
                    .collect();
                if !new_default_grasp_servers.is_empty() {
                    params.grasp_default_set = new_default_grasp_servers;
                }
            }
        }
        params.apply_env_overrides();
        params
    }
}

fn get_dedup_events(relay_results: Vec<Result<Vec<nostr::prelude::Event>>>) -> Vec<Event> {
    let mut dedup_events: Vec<Event> = vec![];
    for events in relay_results.into_iter().flatten() {
        for event in events {
            if !dedup_events.iter().any(|e| event.id.eq(&e.id)) {
                dedup_events.push(event);
            }
        }
    }
    dedup_events
}

pub async fn sign_event(
    event_builder: EventBuilder,
    signer: &Arc<NgitSigner>,
    description: String,
) -> Result<nostr::prelude::Event> {
    signer
        .sign_event_builder_with_description(event_builder, &description)
        .await
        .context("failed to sign event")
}

pub async fn sign_draft_event(
    draft_event: UnsignedEvent,
    signer: &Arc<NgitSigner>,
    description: String,
) -> Result<nostr::prelude::Event> {
    signer
        .sign_event_with_description(draft_event, &description)
        .await
        .context("failed to sign event")
}

pub async fn fetch_public_key(signer: &Arc<NgitSigner>) -> Result<nostr::prelude::PublicKey> {
    if signer.is_remote() {
        let term = console::Term::stderr();
        let progress = TransientLine::write(&term, "fetching npub from remote signer...")?;
        let public_key = signer
            .get_public_key()
            .await
            .context("failed to get npub from remote signer")?;
        progress.clear()?;
        Ok(public_key)
    } else {
        signer
            .get_public_key()
            .await
            .context("failed to get public key from local keys")
    }
}

pub async fn nip05_query(nip05_addr: &str) -> Result<Nip05Profile> {
    let addr_deconstructed = Nip05Address::parse(nip05_addr)
        .context(format!("cannot parse nip05 address: {nip05_addr}"))?;
    nip05_query_address(&addr_deconstructed).await
}

/// As [`nip05_query`], for an address a caller has already parsed.
///
/// # Errors
///
/// Returns an error when the `.well-known` document cannot be fetched, is
/// not JSON, or names no public key for the address.
pub async fn nip05_query_address(nip05_addr: &Nip05Address) -> Result<Nip05Profile> {
    let json_res: Value = crate::tls::http_client_builder()
        .build()
        .context("failed to create the NIP-05 HTTP client")?
        .get(nip05_addr.url().to_string())
        .send()
        .await
        .context(format!(
            "nip05 server is not responding for address: {nip05_addr}"
        ))?
        .json()
        .await
        .context(format!(
            "nip05 server response did not respond with json when querying address: {nip05_addr}"
        ))?;
    Nip05Profile::from_json(nip05_addr, &json_res).context(format!(
        "cannot get public key for nip05 address: {nip05_addr}"
    ))
}

fn pb_style(current_timeout: Arc<AtomicU64>) -> Result<ProgressStyle> {
    Ok(
        ProgressStyle::with_template(" {spinner} {prefix} {msg} {timeout_in}")?.with_key(
            "timeout_in",
            move |state: &ProgressState, w: &mut dyn Write| {
                let elapsed = state.elapsed().as_secs();
                // Each relay displays its own deadline measured from fetch start.
                // Equivalent successful peers may shorten that deadline.
                if elapsed > 3 {
                    let dim = Style::new().color256(247);
                    let timeout = current_timeout.load(Ordering::Relaxed);
                    if elapsed < timeout {
                        write!(
                            w,
                            "{}",
                            dim.apply_to(format!("timeout in {:.1}s", timeout - elapsed))
                                .for_stderr()
                        )
                        .unwrap();
                    }
                }
            },
        ),
    )
}

fn pb_after_style(succeed: bool) -> indicatif::ProgressStyle {
    ProgressStyle::with_template(
        format!(
            " {} {}",
            if succeed {
                console::style("✔".to_string())
                    .for_stderr()
                    .green()
                    .to_string()
            } else {
                console::style("✘".to_string())
                    .for_stderr()
                    .red()
                    .to_string()
            },
            "{prefix} {msg}",
        )
        .as_str(),
    )
    .unwrap()
}

async fn get_local_cache_database(git_repo_path: &Path) -> Result<NostrLmdb> {
    let git_dir = git2::Repository::discover(git_repo_path)
        .context("failed to discover git repository")?
        .commondir()
        .to_path_buf();
    let path = git_dir.join("nostr-cache.lmdb");
    NostrLmdb::open(&path).await.with_context(|| {
        format!(
            "failed to open or create repository nostr cache database at {}; ngit requires the \
             Git common directory to be writable for repository state",
            path.display()
        )
    })
}

fn get_global_cache_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NGIT_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(get_dirs()?.cache_dir().to_path_buf())
}

async fn open_global_cache_database(git_repo_path: Option<&Path>) -> Result<NostrLmdb> {
    let path = if std::env::var("NGITTEST").is_ok() {
        if let Some(git_repo_path) = git_repo_path {
            let git_dir = git2::Repository::discover(git_repo_path)
                .context("failed to discover git repository")?
                .commondir()
                .to_path_buf();
            git_dir.join("test-global-cache.lmdb")
        } else {
            bail!("git_repo must be supplied to get_global_cache_database during integration tests")
        }
    } else {
        let cache_dir = get_global_cache_dir()?;
        create_dir_all(&cache_dir).with_context(|| {
            format!(
                "failed to create global cache directory at {}",
                cache_dir.display()
            )
        })?;
        cache_dir.join("nostr-cache.lmdb")
    };

    NostrLmdb::open(&path).await.with_context(|| {
        format!(
            "failed to open or create global nostr cache database at {}",
            path.display()
        )
    })
}

async fn get_global_cache_database(git_repo_path: Option<&Path>) -> Result<Arc<dyn NostrDatabase>> {
    match open_global_cache_database(git_repo_path).await {
        Ok(database) => Ok(Arc::new(database)),
        Err(error) if std::env::var("NGITTEST").is_err() => Ok(use_in_memory_global_cache(error)),
        Err(error) => Err(error),
    }
}

fn use_in_memory_global_cache(error: anyhow::Error) -> Arc<dyn NostrDatabase> {
    if !GLOBAL_CACHE_WARNING_PRINTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "warning: {error:#}\n\
             continuing with an in-memory global cache; set NGIT_CACHE_DIR to a writable directory \
             to enable persistent caching"
        );
    }
    Arc::clone(GLOBAL_MEMORY_CACHE.get_or_init(|| Arc::new(MemoryDatabase::unbounded())))
}

pub async fn get_events_from_local_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::prelude::Filter>,
) -> Result<Vec<nostr::prelude::Event>> {
    let db = get_local_cache_database(git_repo_path).await?;

    let query_results = join_all(filters.into_iter().map(|filter| async {
        db.query(filter)
            .await
            .context("failed to execute query on opened ngit nostr cache database")
    }))
    .await;

    // no Event is being mutated, just new items added to the set
    #[allow(clippy::mutable_key_type)]
    let mut events: HashSet<Event> = HashSet::new();

    for result in query_results {
        events.extend(result?);
    }

    Ok(events.into_iter().collect())
}

pub async fn get_event_from_global_cache(
    git_repo_path: Option<&Path>,
    filters: Vec<nostr::prelude::Filter>,
) -> Result<Vec<nostr::prelude::Event>> {
    let db = get_global_cache_database(git_repo_path).await?;

    let query_results = join_all(filters.into_iter().map(|filter| async {
        db.query(filter)
            .await
            .context("failed to execute query on opened ngit nostr cache database")
    }))
    .await;

    // no Event is being mutated, just new items added to the set
    #[allow(clippy::mutable_key_type)]
    let mut events: HashSet<Event> = HashSet::new();

    for result in query_results {
        events.extend(result?);
    }

    Ok(events.into_iter().collect())
}

pub async fn save_event_in_local_cache(
    git_repo_path: &Path,
    event: &nostr::prelude::Event,
) -> Result<bool> {
    match get_local_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("failed to save event in local cache")?
    {
        SaveEventStatus::Success => Ok(true),
        _ => Ok(false),
    }
}

/// Fetch arbitrary filters from each relay and cache every successful result.
///
/// The repository-wide fetch plan intentionally knows only about repository,
/// proposal, issue, and profile events. Feature-specific event families such
/// as NIP-82 releases need a small, explicit escape hatch which retains the
/// relay associated with each success or failure. The shared relay progress
/// report keeps this path consistent with repository-wide fetches. Read
/// commands can merge the successful routes while reporting incomplete
/// discovery; mutation preflights can fail closed when any required
/// publication route was not queried completely.
pub async fn fetch_filters_to_local_cache(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: &Path,
    relays: &[RelayUrl],
    filters: &[Filter],
) -> Result<Vec<(RelayUrl, Result<Vec<Event>>)>> {
    let progress_reporter = RelayProgressReporter::fetching();
    let progress = progress_reporter.handle();
    let results = join_all(relays.iter().cloned().map(|relay| {
        let progress = progress.clone();
        async move {
            let result = async {
                let mut relay_results = client
                    .get_events_per_relay(vec![relay.clone()], filters.to_vec(), progress)
                    .await
                    .with_context(|| format!("failed to query relay {relay}"))?;
                if relay_results.len() != 1 {
                    bail!(
                        "relay {relay} did not produce exactly one query result (got {})",
                        relay_results.len()
                    );
                }
                let events = relay_results
                    .pop()
                    .expect("length was checked")
                    .with_context(|| format!("failed to fetch events from {relay}"))?;
                for event in &events {
                    save_event_in_local_cache(git_repo_path, event).await?;
                }
                Ok(events)
            }
            .await;
            (relay, result)
        }
    }))
    .await;
    progress_reporter.finish(
        results.iter().any(|(_, result)| result.is_err()),
        !results.is_empty() && results.iter().all(|(_, result)| result.is_err()),
        None,
    )?;
    Ok(results)
}

pub async fn save_event_in_global_cache(
    git_repo_path: Option<&Path>,
    event: &nostr::prelude::Event,
) -> Result<bool> {
    match get_global_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("failed to save event in global cache")
    {
        Ok(SaveEventStatus::Success) => Ok(true),
        Ok(_) => Ok(false),
        Err(e) => Err(e).context("failed to save event in global cache"),
    }
}

/// Reduce announcements to the canonical event per author - NIP-01
/// addressable-event rules: greatest `created_at`, lowest event id on a
/// tie - sorted by `created_at` ascending with the lower id last on
/// cross-author ties, so `.last()` is the event addressable rules select.
fn latest_announcement_per_author(
    events: Vec<nostr::prelude::Event>,
) -> Vec<nostr::prelude::Event> {
    let mut latest: HashMap<PublicKey, nostr::prelude::Event> = HashMap::new();
    for event in events {
        let supersedes = latest.get(&event.pubkey).is_none_or(|existing| {
            event.created_at > existing.created_at
                || (event.created_at == existing.created_at && event.id < existing.id)
        });
        if supersedes {
            latest.insert(event.pubkey, event);
        }
    }
    let mut events: Vec<nostr::prelude::Event> = latest.into_values().collect();
    events.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    events
}

// use annoucement from selected maintainer but recursively add maintainers, git
// servers and relays
pub async fn get_repo_ref_from_cache(
    git_repo_path: Option<&Path>,
    repo_coordinate: &Nip19Coordinate,
) -> Result<RepoRef> {
    get_repo_ref_from_cache_with_selected_recovery(git_repo_path, repo_coordinate, false).await
}

/// Resolve enough of an unconfirmed selected coordinate's explicit lead path
/// for `ngit repo follow-lead` to move the checkout to the authoritative lead.
/// No other command should opt into this recovery view.
pub async fn get_repo_ref_from_cache_for_lead_recovery(
    git_repo_path: Option<&Path>,
    repo_coordinate: &Nip19Coordinate,
) -> Result<RepoRef> {
    get_repo_ref_from_cache_with_selected_recovery(git_repo_path, repo_coordinate, true).await
}

async fn get_repo_ref_from_cache_with_selected_recovery(
    git_repo_path: Option<&Path>,
    repo_coordinate: &Nip19Coordinate,
    allow_unconfirmed_selected: bool,
) -> Result<RepoRef> {
    // pubkeys whose announcements are fetched: per NIP-34 clients SHOULD
    // recursively fetch announcements from each pubkey assigned a role, so
    // `o`-assigned moderators are fetched alongside the maintainer listing —
    // their announcement carries their acknowledgement or leave self-entries
    let mut discovered = HashSet::new();
    let mut maintainers = HashSet::new();
    let mut ordered_maintainers = Vec::new();
    let mut new_discovery: bool;

    discovered.insert(repo_coordinate.public_key);
    maintainers.insert(repo_coordinate.public_key);
    ordered_maintainers.push(repo_coordinate.public_key);
    let mut repo_events = vec![];
    loop {
        new_discovery = false;
        let repo_events_filter = get_filter_repo_ann_events(
            &HashSet::from_iter(discovered.iter().map(|m| Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: *m,
                    identifier: repo_coordinate.identifier.to_string(),
                },
                relays: vec![],
            })),
            true,
        );

        let events = [
            get_event_from_global_cache(git_repo_path, vec![repo_events_filter.clone()]).await?,
            if let Some(git_repo_path) = git_repo_path {
                get_events_from_local_cache(git_repo_path, vec![repo_events_filter]).await?
            } else {
                vec![]
            },
        ]
        .concat();
        for e in events {
            if let Ok(repo_ref) = RepoRef::try_from((e.clone(), None)) {
                // only a maintainer-listed author's listings expand the sets:
                // a moderator's (or any other role-fetched pubkey's)
                // assignments assign nothing, so their announcement is
                // consulted solely for their own self-entries
                if maintainers.contains(&e.pubkey) {
                    for m in repo_ref.maintainers {
                        if maintainers.insert(m) {
                            ordered_maintainers.push(m);
                            new_discovery = true;
                        }
                        discovered.insert(m);
                    }
                    for moderator in repo_ref.moderators {
                        if discovered.insert(moderator) {
                            new_discovery = true;
                        }
                    }
                }
                repo_events.push(e);
            }
        }
        if !new_discovery {
            break;
        }
    }
    // NIP-01 addressable-event rules: only an author's latest announcement
    // speaks for them. The global and per-repo caches can return different
    // versions and the discovery loop re-collects every iteration, so
    // reduce to the canonical event per author before consolidating -
    // otherwise a stale version could keep an ended role assignment active
    // or hide a newer acknowledgement, leave or return.
    let repo_events = latest_announcement_per_author(repo_events);
    // A member's own announcement takes precedence over assignments in other
    // members' announcements: an author with role entries but no active
    // `M`/`m` self-entry left the maintainer set or acknowledges only
    // moderatorship, so drop them from the maintainer set (and with it the
    // pooling of their infrastructure). Keep malformed self-`defer` authors
    // and authors whose self-records are exclusively unparseable as
    // discovery-only candidates: confirmed_maintainers() still denies
    // them authority, while health reporting needs their signed event —
    // garbage records a departure no more than a `defer` sentinel does.
    let declined_maintainers: HashSet<PublicKey> = repo_events
        .iter()
        .filter(|e| {
            let invalid = announcement_invalid_self_defers(e);
            announcement_author_validly_declines_maintainership(e)
                || (announcement_author_declines_maintainership(e)
                    && invalid.is_empty()
                    && !announcement_author_has_only_malformed_self_records(e))
        })
        .map(|e| e.pubkey)
        .collect();
    ordered_maintainers.retain(|m| !declined_maintainers.contains(m));
    let repo_ref = RepoRef::try_from((
        repo_events
            .iter()
            .find(|e| e.pubkey == repo_coordinate.public_key)
            .context("no repo announcement event found at specified Nip19Coordinates. if you are the repository maintainer consider running `ngit init` to create one")?
            .clone(),
        Some(repo_coordinate.public_key),
    ))?;

    let mut events: HashMap<Nip19Coordinate, nostr::prelude::Event> = HashMap::new();
    for m in &ordered_maintainers {
        if let Some(e) = repo_events.iter().find(|e| e.pubkey.eq(m)) {
            events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: e.kind,
                        identifier: e.tags.identifier().unwrap().to_string(),
                        public_key: e.pubkey,
                    },
                    relays: vec![],
                },
                e.clone(),
            );
        }
    }
    if !events
        .values()
        .any(|event| event.pubkey == repo_coordinate.public_key)
    {
        // Keep a departed selected author's event for self-role and forwarding
        // analysis, but never as authoritative repository data. Without this
        // discovery-only copy confirmed_maintainers() would not see the ended
        // self-role and would incorrectly seed the selected author again.
        if let Some(e) = repo_events
            .iter()
            .find(|event| event.pubkey == repo_coordinate.public_key)
        {
            events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: e.kind,
                        identifier: e.tags.identifier().unwrap().to_string(),
                        public_key: e.pubkey,
                    },
                    relays: vec![],
                },
                e.clone(),
            );
        }
    }
    // Invalid self-`defer` and exclusively-malformed announcements are
    // retained strictly for health and explicit repair, even when a
    // separate valid record proves the author departed. Candidate and
    // authority sets remain controlled by the resolved role graph, not by
    // presence in this map. These are the least trustworthy retained
    // events, so a missing identifier skips the event instead of panicking.
    for event in &repo_events {
        if !announcement_invalid_self_defers(event).is_empty()
            || announcement_author_has_only_malformed_self_records(event)
        {
            let Some(identifier) = event.tags.identifier() else {
                continue;
            };
            events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: event.kind,
                        identifier: identifier.to_string(),
                        public_key: event.pubkey,
                    },
                    relays: vec![],
                },
                event.clone(),
            );
        }
    }

    // also set maintainers_without_annoucnement
    let mut maintainers_without_annoucnement: Vec<PublicKey> = vec![];

    for m in &ordered_maintainers {
        if !repo_events.iter().any(|e| e.pubkey == *m) {
            maintainers_without_annoucnement.push(*m);
        }
    }

    let mut ordered_accepted_maintainers = Vec::new();
    let mut ordered_requested_maintainers = Vec::new();
    let requested_maintainers: HashSet<PublicKey> =
        maintainers_without_annoucnement.iter().copied().collect();
    for maintainer in ordered_maintainers {
        if requested_maintainers.contains(&maintainer) {
            ordered_requested_maintainers.push(maintainer);
        } else {
            ordered_accepted_maintainers.push(maintainer);
        }
    }
    let ordered_maintainers =
        [ordered_accepted_maintainers, ordered_requested_maintainers].concat();

    let mut repo_ref = RepoRef {
        // use all maintainers from all events found, not just maintainers in the most
        // recent event
        maintainers: ordered_maintainers,
        moderators: vec![],
        // Shared fields are filled only after the reciprocal graph has
        // identified the confirmed member component below.
        relays: vec![],
        git_server: vec![],
        blossoms: vec![],
        events,
        maintainers_without_annoucnement: Some(maintainers_without_annoucnement),
        private: false,
        ..repo_ref
    };

    // Readability carve-out: skip the follow-lead bail only when the
    // selected author is unconfirmed *because* their own records are broken
    // — a blocking invalid self-`defer` or exclusively malformed
    // self-records — and no valid numeric departure exists. A validly
    // departed author redirects through follow-lead as usual: an incidental
    // stray `defer` must not keep the CLI silently operating on the
    // superseded coordinate.
    let selected_unconfirmed_by_broken_records =
        selected_author_is_unconfirmed_by_broken_records(&repo_ref, repo_coordinate.public_key);
    if !allow_unconfirmed_selected
        && !selected_unconfirmed_by_broken_records
        && !repo_ref
            .confirmed_maintainers()
            .contains(&repo_coordinate.public_key)
    {
        bail!(
            "the selected repository coordinate author is no longer a confirmed maintainer; run `ngit repo follow-lead` to switch to the active lead"
        );
    }

    // `o` role assignments only carry authority from `M`/`m` members
    // (RepoRef::assigned_moderators), which needs the consolidated events
    // map, so the moderator set is computed last. A member's own
    // announcement takes precedence over `o` assignments in others': an
    // author whose fetched announcement records only ended `o` self-entries
    // left moderatorship (e.g. via `ngit repo leave`). Their announcement —
    // discovered by following the `o` assignment — sits outside the
    // membership graph's events map, so the fetched events are consulted
    // directly. As above, retain malformed self-`defer` candidates so their
    // author-scoped health and explicit repair path remain available.
    let declined_moderators: HashSet<PublicKey> = repo_events
        .iter()
        .filter(|e| {
            let invalid = announcement_invalid_self_defers(e);
            announcement_author_validly_declines_moderatorship(e)
                || (announcement_author_declines_moderatorship(e)
                    && invalid.is_empty()
                    && !announcement_author_has_only_malformed_self_records(e))
        })
        .map(|e| e.pubkey)
        .collect();
    repo_ref.moderators = repo_ref
        .assigned_moderators()
        .into_iter()
        .filter(|m| !declined_moderators.contains(m))
        .collect();
    // moderators' own announcements join the events map so their
    // acknowledgement can be evaluated (RepoRef::confirmed_moderators)
    for moderator in repo_ref.moderators.clone() {
        if let Some(e) = repo_events.iter().find(|e| e.pubkey == moderator) {
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: e.kind,
                        identifier: e.tags.identifier().unwrap().to_string(),
                        public_key: e.pubkey,
                    },
                    relays: vec![],
                },
                e.clone(),
            );
        }
    }

    apply_confirmed_member_repository_data(&mut repo_ref);

    Ok(repo_ref)
}

/// Whether the selected author's retained announcement excludes them from
/// confirmation *because* of broken records — a blocking invalid
/// self-`defer` or exclusively malformed self-records — rather than a valid
/// signed departure. Only this shape justifies read-only fallbacks: a
/// validly departed author must redirect via `ngit repo follow-lead` even
/// when a stray invalid record sits beside the numeric departure.
fn selected_author_is_unconfirmed_by_broken_records(
    repo_ref: &RepoRef,
    selected: PublicKey,
) -> bool {
    repo_ref.events.values().any(|event| {
        event.pubkey == selected
            && !announcement_author_validly_declines_maintainership(event)
            && (announcement_invalid_self_defers(event)
                .iter()
                .any(|invalid| invalid.blocks_author())
                || announcement_author_has_only_malformed_self_records(event))
    })
}

/// Apply shared repository fields using confirmed members only.
///
/// `RepoRef::events` also retains invitation announcements because the graph
/// resolver needs them to recognize acceptance. Those events must not alter
/// metadata, privacy or infrastructure until their author is confirmed. The
/// sole exception is an unresolved selected coordinate whose own records are
/// broken (a blocking invalid self-`defer` or exclusively malformed
/// self-records): when there is no confirmed member at all, retain that
/// selected event's signed fields so read-only inspection remains possible.
/// It still grants no member or state authority.
fn apply_confirmed_member_repository_data(repo_ref: &mut RepoRef) {
    let authoritative_events: Vec<Event> = repo_ref
        .confirmed_member_announcements()
        .into_iter()
        .cloned()
        .collect();
    let latest_metadata = authoritative_events
        .last()
        .and_then(|event| RepoRef::try_from((event.clone(), None)).ok());

    if authoritative_events.is_empty() {
        let unresolved_selected = selected_author_is_unconfirmed_by_broken_records(
            repo_ref,
            repo_ref.selected_maintainer,
        )
        .then(|| {
            repo_ref
                .events
                .values()
                .find(|event| event.pubkey == repo_ref.selected_maintainer)
                .and_then(|event| RepoRef::try_from((event.clone(), None)).ok())
        })
        .flatten();
        if let Some(selected) = unresolved_selected {
            repo_ref.name = selected.name;
            repo_ref.description = selected.description;
            repo_ref.web = selected.web;
            repo_ref.upstream = selected.upstream;
            repo_ref.hashtags = selected.hashtags;
            repo_ref.private = selected.private;
            repo_ref.relays = selected.relays;
            repo_ref.git_server = selected.git_server;
            repo_ref.blossoms = selected.blossoms;
            return;
        }
    }

    let mut relays = Vec::new();
    let mut git_server = Vec::new();
    let mut blossoms = Vec::new();
    let mut seen_relays = HashSet::new();
    let mut seen_git_server = HashSet::new();
    let mut seen_blossoms = HashSet::new();

    for member in repo_ref.confirmed_members() {
        let Some(event) = authoritative_events
            .iter()
            .find(|event| event.pubkey == member)
        else {
            continue;
        };
        let Ok(member_ref) = RepoRef::try_from((event.clone(), None)) else {
            continue;
        };
        for relay in member_ref.relays {
            if seen_relays.insert(relay.clone()) {
                relays.push(relay);
            }
        }
        for server in member_ref.git_server {
            if seen_git_server.insert(server.trim_end_matches('/').to_string()) {
                git_server.push(server);
            }
        }
        for blossom in member_ref.blossoms {
            if seen_blossoms.insert(blossom.clone()) {
                blossoms.push(blossom);
            }
        }
    }

    if let Some(metadata) = latest_metadata {
        repo_ref.name = metadata.name;
        repo_ref.description = metadata.description;
        repo_ref.web = metadata.web;
        repo_ref.upstream = metadata.upstream;
        repo_ref.hashtags = metadata.hashtags;
    }
    repo_ref.private = repository_events_are_private(&authoritative_events);
    repo_ref.relays = relays;
    repo_ref.git_server = git_server;
    repo_ref.blossoms = blossoms;
}

/// Record the repository's privacy classification in `.git/config` so later
/// operations can consult it before any network access.
///
/// Only call this when `git_repo_path` belongs to the repository the
/// operation actually targets: merely browsing another repository's
/// announcement (e.g. during interactive repository search) must not stamp
/// its privacy into the current repository's config. Best-effort: the key is
/// only written when the value changes, and failures degrade to a warning so
/// read-only flows never fail on config write access.
pub fn save_repository_privacy_to_git_config(git_repo_path: &Path, private: bool) {
    let result = (|| -> Result<()> {
        let repository =
            git2::Repository::discover(git_repo_path).context("failed to discover repository")?;
        let mut config = repository
            .config()
            .context("failed to open repository config")?;
        if config.get_bool("nostr.private").ok() == Some(private) {
            return Ok(());
        }
        config
            .set_bool("nostr.private", private)
            .context("failed to set nostr.private")?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("warning: failed to record repository privacy in git config: {error:#}");
    }
}

fn repository_events_are_private(events: &[Event]) -> bool {
    events.iter().any(|event| {
        RepoRef::try_from((event.clone(), None)).is_ok_and(|repo_ref| repo_ref.private)
    })
}

pub async fn warn_if_invited_as_maintainer(git_repo_path: &Path, repo_ref: &RepoRef) {
    if INVITED_MAINTAINER_WARNING_PRINTED.load(Ordering::Relaxed) {
        return;
    }

    let invited_as_maintainer = match get_likely_logged_in_user(git_repo_path).await {
        Ok(Some(pubkey)) => repo_ref
            .maintainers_without_annoucnement
            .as_ref()
            .is_some_and(|without| without.contains(&pubkey)),
        Ok(None) | Err(_) => false,
    };

    if invited_as_maintainer && !INVITED_MAINTAINER_WARNING_PRINTED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: you are invited as a maintainer, consider running `ngit repo accept`.");
    }
}

pub async fn get_state_from_cache(
    git_repo_path: Option<&Path>,
    repo_ref: &RepoRef,
) -> Result<RepoState> {
    let events = if let Some(git_repo_path) = git_repo_path {
        get_events_from_local_cache(
            git_repo_path,
            vec![get_filter_state_events(&repo_ref.coordinates(), true)],
        )
        .await?
    } else {
        get_event_from_global_cache(
            git_repo_path,
            vec![get_filter_state_events(&repo_ref.coordinates(), true)],
        )
        .await?
    };
    // state events are only authoritative from confirmed maintainers; invited
    // maintainers' state events are ignored until they accept
    let authorized_state_authors = repo_ref.confirmed_maintainers();
    RepoState::try_from(
        events
            .into_iter()
            .filter(|event| authorized_state_authors.contains(&event.pubkey))
            .collect::<Vec<Event>>(),
    )
}

#[allow(clippy::too_many_lines)]
async fn create_relays_request(
    git_repo_path: Option<&Path>,
    selected_maintainer_coordinate: Option<&Nip19Coordinate>,
    user_profiles: &HashSet<PublicKey>,
    private_relay_list_authors: &HashSet<PublicKey>,
    fallback_relays: HashSet<RelayUrl>,
    announcement_indexer_relays: HashSet<RelayUrl>,
    repository_relays_only: bool,
) -> Result<FetchRequest> {
    let repo_ref = if let Some(selected_maintainer_coordinate) = selected_maintainer_coordinate {
        (get_repo_ref_from_cache(git_repo_path, selected_maintainer_coordinate).await).ok()
    } else {
        None
    };
    let cached_repository_is_private = repo_ref.as_ref().is_some_and(|repo_ref| repo_ref.private);
    let repo_auth_mode = if cached_repository_is_private {
        RelayAuthMode::Required
    } else {
        RelayAuthMode::IfSignerAttached
    };
    let repository_relays_only =
        restrict_repository_relays(repository_relays_only, cached_repository_is_private);
    // NIP-AD permits relay hints to be omitted. Bootstrap the selected
    // author's NIP-65 relay list on the ordinary announcement indexers, then
    // treat its write relays as additional announcement locations below.
    // Applying this to every hint-less coordinate also improves the equivalent
    // npub/identifier URL without changing hinted or private discovery.
    let hintless_coordinate_author = (!repository_relays_only && repo_ref.is_none())
        .then(|| selected_maintainer_coordinate.filter(|coordinate| coordinate.relays.is_empty()))
        .flatten()
        .map(|coordinate| coordinate.public_key);
    let lock_repository_relays = repository_relays_only
        && coordinate_hints_are_allowed(cached_repository_is_private)
        && selected_maintainer_coordinate.is_some_and(|coordinate| !coordinate.relays.is_empty());

    let repo_coordinates = {
        // add Nip19Coordinates of users listed in maintainers to explicitly
        // specified coodinates
        let mut set: HashSet<Nip19Coordinate> = HashSet::new();
        if let Some(selected_maintainer_coordinate) = selected_maintainer_coordinate {
            set.insert(selected_maintainer_coordinate.clone());
        }
        if let Some(repo_ref) = &repo_ref {
            for c in repo_ref.coordinates() {
                if !set
                    .iter()
                    .any(|e| e.identifier.eq(&c.identifier) && e.public_key.eq(&c.public_key))
                {
                    set.insert(c);
                }
            }
        }
        set
    };

    let repo_coordinates_without_relays = {
        let mut set = HashSet::new();
        for c in &repo_coordinates {
            set.insert(Nip19Coordinate {
                coordinate: Coordinate {
                    kind: c.kind,
                    identifier: c.identifier.clone(),
                    public_key: c.public_key,
                },
                relays: vec![],
            });
        }
        set
    };

    let mut proposals: HashSet<EventId> = HashSet::new();
    let mut issue_ids: HashSet<EventId> = HashSet::new();
    let mut missing_contributor_profiles: HashSet<PublicKey> = HashSet::new();
    let mut contributors: HashSet<PublicKey> = HashSet::new();

    if !repo_coordinates_without_relays.is_empty() {
        if let Some(repo_ref) = &repo_ref {
            for m in &repo_ref.maintainers {
                contributors.insert(m.to_owned());
            }
        }

        if let Some(git_repo_path) = git_repo_path {
            for event in &get_events_from_local_cache(
                git_repo_path,
                vec![
                    nostr::prelude::Filter::default()
                        .kinds(vec![Kind::GitPatch, KIND_PULL_REQUEST, Kind::GitIssue])
                        .custom_tags(
                            SingleLetterTag::LOWERCASE_A,
                            repo_coordinates_without_relays
                                .iter()
                                .map(|c| c.coordinate.to_string())
                                .collect::<Vec<String>>(),
                        ),
                ],
            )
            .await?
            {
                if event_is_patch_set_root(event)
                    || event_is_revision_root(event)
                    || event.kind.eq(&KIND_PULL_REQUEST)
                {
                    proposals.insert(event.id);
                    contributors.insert(event.pubkey);
                } else if event.kind.eq(&Kind::GitIssue) {
                    issue_ids.insert(event.id);
                    contributors.insert(event.pubkey);
                }
            }
        }

        let profile_events = get_event_from_global_cache(
            git_repo_path,
            vec![get_filter_contributor_profiles(contributors.clone())],
        )
        .await?;
        for c in &contributors {
            if let Some(event) = profile_events
                .iter()
                .find(|e| e.kind == Kind::Metadata && e.pubkey.eq(c))
            {
                if let Some(git_repo_path) = git_repo_path {
                    save_event_in_local_cache(git_repo_path, event).await?;
                }
            } else {
                missing_contributor_profiles.insert(c.to_owned());
            }
        }
    }

    let profiles_to_fetch_from_user_relays = {
        let mut user_profiles = user_profiles.clone();
        user_profiles.extend(private_relay_list_authors.iter().copied());
        user_profiles.extend(hintless_coordinate_author);
        if let Some(git_repo_path) = git_repo_path {
            if let Ok(Some(current_user)) = get_likely_logged_in_user(git_repo_path).await {
                user_profiles.insert(current_user);
            }
        }
        let mut map: HashMap<PublicKey, (Timestamp, Timestamp, Timestamp)> = HashMap::new();
        for public_key in &user_profiles {
            if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
                map.insert(
                    public_key.to_owned(),
                    (
                        user_ref.metadata.created_at,
                        user_ref.relays.created_at,
                        user_ref.grasp_list.created_at,
                    ),
                );
            } else {
                map.insert(
                    public_key.to_owned(),
                    (Timestamp::from(0), Timestamp::from(0), Timestamp::from(0)),
                );
            }
        }
        map
    };

    let user_relays_for_profiles = if repository_relays_only {
        HashSet::new()
    } else {
        let mut set = HashSet::new();
        for user in &profiles_to_fetch_from_user_relays
            .clone()
            .into_keys()
            .collect::<Vec<PublicKey>>()
        {
            if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, user).await {
                for r in user_ref.relays.write() {
                    if let Ok(url) = RelayUrl::parse(&r) {
                        set.insert(url);
                    }
                }
            } else {
                missing_contributor_profiles.insert(user.to_owned());
            }
        }
        set
    };

    let existing_events: HashSet<EventId> = {
        let mut existing_events: HashSet<EventId> = HashSet::new();
        for filter in get_fetch_filters(
            &repo_coordinates_without_relays,
            &proposals,
            &issue_ids,
            &HashSet::new(), /* non_proposal_event_ids not yet computed; deletion events are not
                              * cached locally */
            &missing_contributor_profiles
                .union(
                    &profiles_to_fetch_from_user_relays
                        .clone()
                        .into_keys()
                        .collect::<HashSet<PublicKey>>(),
                )
                .copied()
                .collect(),
        ) {
            if let Some(git_repo_path) = git_repo_path {
                for (id, _) in get_local_cache_database(git_repo_path)
                    .await?
                    .negentropy_items(filter.clone())
                    .await?
                {
                    existing_events.insert(id);
                }
            }
            // Also check global cache for profile events to avoid re-fetching
            if filter.kinds.as_ref().is_some_and(|kinds| {
                kinds.iter().any(|k| {
                    k.eq(&Kind::Metadata) || k.eq(&Kind::RelayList) || k.eq(&KIND_USER_GRASP_LIST)
                })
            }) {
                for (id, _) in get_global_cache_database(git_repo_path)
                    .await?
                    .negentropy_items(filter)
                    .await?
                {
                    existing_events.insert(id);
                }
            }
        }
        existing_events
    };

    let repo_relays = {
        // With repository context, only relays from a cached announcement are
        // authoritative for state and collaboration events. A repository-only
        // probe without a cached private announcement comes from decrypted
        // kind-10318 hints, which are authoritative for that discovery pass.
        let mut relays = if selected_maintainer_coordinate.is_none() && !repository_relays_only {
            fallback_relays.clone()
        } else {
            HashSet::new()
        };
        if !lock_repository_relays {
            if let Some(repo_ref) = &repo_ref {
                for r in repo_ref.relays.clone() {
                    relays.insert(r);
                }
            }
        }
        if lock_repository_relays {
            relays.extend(
                repo_coordinates
                    .iter()
                    .flat_map(|coordinate| coordinate.relays.iter().cloned()),
            );
        }
        relays
    };

    let announcement_indexer_relays = {
        if repo_coordinates_without_relays.is_empty() || repository_relays_only {
            HashSet::new()
        } else {
            // URL/naddr relay hints locate announcements; they do not become
            // authorities for repository events until an announcement lists
            // them. A bare coordinate additionally uses fallback relays for
            // announcement bootstrap only.
            let coordinate_hint_relays = repo_coordinates
                .iter()
                .flat_map(|coordinate| coordinate.relays.iter().cloned())
                .collect::<HashSet<_>>();
            let mut relays = announcement_indexer_relays;
            relays.extend(coordinate_hint_relays.iter().cloned());
            if let Some(author) = hintless_coordinate_author {
                if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, &author).await {
                    relays.extend(
                        user_ref
                            .relays
                            .write()
                            .into_iter()
                            .filter_map(|relay| RelayUrl::parse(&relay).ok()),
                    );
                }
            }
            if repo_relays.is_empty() && coordinate_hint_relays.is_empty() {
                relays.extend(fallback_relays);
            }
            relays
        }
    };
    let author_announcement_relays = if let Some(author) = hintless_coordinate_author {
        if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, &author).await {
            user_ref
                .relays
                .write()
                .into_iter()
                .filter_map(|relay| RelayUrl::parse(&relay).ok())
                .collect()
        } else {
            HashSet::new()
        }
    } else {
        HashSet::new()
    };

    let relay_column_width = repo_relays
        .union(&user_relays_for_profiles)
        .chain(announcement_indexer_relays.iter())
        .reduce(|a, r| {
            if r.to_string()
                .chars()
                .count()
                .gt(&a.to_string().chars().count())
            {
                r
            } else {
                a
            }
        })
        .map_or(0, |r| r.to_string().chars().count() + 2);

    Ok(FetchRequest {
        selected_relay: None,
        repo_relays,
        announcement_indexer_relays,
        author_announcement_relays,
        announcement_profile_authors: hintless_coordinate_author.into_iter().collect(),
        scope: RelayFetchScope::Repository,
        repo_auth_mode,
        repository_relays_only,
        lock_repository_relays,
        relay_column_width,
        repo_coordinates_without_relays: if let Some(repo_ref) = &repo_ref {
            repo_ref.coordinates_with_timestamps()
        } else {
            repo_coordinates_without_relays
                .iter()
                .map(|c| (c.clone(), None))
                .collect()
        },
        maintainer_listed_authors: {
            let mut authors: HashSet<PublicKey> = HashSet::new();
            if let Some(coordinate) = selected_maintainer_coordinate {
                authors.insert(coordinate.public_key);
            }
            if let Some(repo_ref) = &repo_ref {
                authors.extend(repo_ref.maintainers.iter().copied());
            }
            authors
        },
        state: if let Some(repo_ref) = &repo_ref {
            if let Ok(existing_state) = get_state_from_cache(git_repo_path, repo_ref).await {
                Some((existing_state.event.created_at, existing_state.event.id))
            } else {
                None
            }
        } else {
            None
        },
        non_proposal_event_ids: {
            let mut ids: HashSet<EventId> = HashSet::new();
            // Include repo announcement event IDs so we can request kind-5
            // deletions for them by #e tag (NIP-09 style).
            if let Some(repo_ref) = &repo_ref {
                for event in repo_ref.events.values() {
                    ids.insert(event.id);
                }
                // Also include the state event ID if we have one.
                if let Ok(existing_state) = get_state_from_cache(git_repo_path, repo_ref).await {
                    ids.insert(existing_state.event.id);
                }
            }
            ids
        },
        proposals,
        issue_ids,
        contributors,
        missing_contributor_profiles,
        existing_events,
        profiles_to_fetch_from_user_relays,
        user_relays_for_profiles,
        private_relay_list_authors: private_relay_list_authors.clone(),
    })
}

fn restrict_repository_relays(requested: bool, cached_repository_is_private: bool) -> bool {
    requested || cached_repository_is_private
}

fn coordinate_hints_are_allowed(cached_repository_is_private: bool) -> bool {
    !cached_repository_is_private
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn process_fetched_events(
    events: Vec<nostr::prelude::Event>,
    request: &FetchRequest,
    git_repo_path: Option<&Path>,
    fresh_coordinates: &mut HashSet<Nip19Coordinate>,
    fresh_proposal_roots: &mut HashSet<EventId>,
    fresh_issue_roots: &mut HashSet<EventId>,
    fresh_profiles: &mut HashSet<PublicKey>,
    maintainer_listed: &mut HashSet<PublicKey>,
    session_announcements: &mut Vec<AnnouncementListing>,
    report: &mut FetchReport,
) -> Result<()> {
    for event in &events {
        if !request.existing_events.contains(&event.id) {
            let is_background_update_event = version_check::is_update_check_event(event)
                && (!version_check::is_ngit_repo_state_event(event)
                    || !request_includes_ngit_repo(request));
            if !is_background_update_event && event.kind != KIND_PRIVATE_GIT_RELAY_LIST {
                if let Some(git_repo_path) = git_repo_path {
                    save_event_in_local_cache(git_repo_path, event).await?;
                }
            }
            if version_check::is_update_check_event(event) {
                save_event_in_global_cache(git_repo_path, event).await?;
                if is_background_update_event {
                    continue;
                }
            }
            if event.kind == KIND_PRIVATE_GIT_RELAY_LIST {
                save_event_in_global_cache(git_repo_path, event).await?;
            } else if event.kind.eq(&Kind::GitRepoAnnouncement) {
                save_event_in_global_cache(git_repo_path, event).await?;
                let new_coordinate = !request
                    .repo_coordinates_without_relays
                    .iter()
                    .map(|(c, _)| c.clone())
                    .any(|c| {
                        c.identifier.eq(event.tags.identifier().unwrap().as_str())
                            && c.public_key.eq(&event.pubkey)
                    });
                let update_to_existing = !new_coordinate
                    && request
                        .repo_coordinates_without_relays
                        .iter()
                        .any(|(c, t)| {
                            c.identifier.eq(event.tags.identifier().unwrap().as_str())
                                && c.public_key.eq(&event.pubkey)
                                && if let Some(t) = t {
                                    event.created_at.gt(t)
                                } else {
                                    true
                                }
                        });
                if update_to_existing {
                    report.updated_repo_announcements.push((
                        Nip19Coordinate {
                            coordinate: Coordinate {
                                kind: event.kind,
                                public_key: event.pubkey,
                                identifier: event.tags.identifier().unwrap().to_owned(),
                            },
                            relays: vec![],
                        },
                        event.created_at,
                    ));
                }
                // if contains announcement
                if let Ok(repo_ref) = &RepoRef::try_from((event.clone(), None)) {
                    // recorded for the whole session; whether this
                    // announcement's listings expand discovery is decided in
                    // expand_role_discovery once the batch is complete
                    session_announcements.push(AnnouncementListing {
                        author: event.pubkey,
                        identifier: repo_ref.identifier.clone(),
                        maintainers: repo_ref.maintainers.clone(),
                        moderators: repo_ref.moderators.clone(),
                    });
                }
            } else if event.kind.eq(&STATE_KIND) {
                let existing_state = if report.updated_state.is_some() {
                    report.updated_state
                } else {
                    request.state
                };
                if let Some((timestamp, id)) = existing_state {
                    if event.created_at.gt(&timestamp)
                        || (event.created_at.eq(&timestamp) && event.id.gt(&id))
                    {
                        report.updated_state = Some((event.created_at, event.id));
                    }
                }
            } else if event.kind.eq(&Kind::EventDeletion) {
                report.deletions += 1;
            } else if event_is_patch_set_root(event) || event.kind.eq(&KIND_PULL_REQUEST) {
                fresh_proposal_roots.insert(event.id);
                report.proposals.insert(event.id);
                if !request.contributors.contains(&event.pubkey)
                    && !fresh_profiles.contains(&event.pubkey)
                {
                    fresh_profiles.insert(event.pubkey);
                }
            } else if event.kind.eq(&Kind::GitIssue) {
                fresh_issue_roots.insert(event.id);
                report.issues.insert(event.id);
                if !request.contributors.contains(&event.pubkey)
                    && !fresh_profiles.contains(&event.pubkey)
                {
                    fresh_profiles.insert(event.pubkey);
                }
            } else if event.kind.eq(&KIND_COMMENT) {
                report.comments.insert(event.id);
            } else if event.kind.eq(&KIND_LABEL) {
                report.labels.insert(event.id);
            } else if event.kind.eq(&KIND_COVER_NOTE) {
                report.cover_notes.insert(event.id);
            } else if [Kind::RelayList, Kind::Metadata, KIND_USER_GRASP_LIST].contains(&event.kind)
            {
                if request.missing_contributor_profiles.contains(&event.pubkey) {
                    report.contributor_profiles.insert(event.pubkey);
                } else if let Some((
                    _,
                    (metadata_timestamp, relay_list_timestamp, grasp_list_timestamp),
                )) = request
                    .profiles_to_fetch_from_user_relays
                    .get_key_value(&event.pubkey)
                {
                    if (Kind::Metadata.eq(&event.kind) && event.created_at.gt(metadata_timestamp))
                        || (Kind::RelayList.eq(&event.kind)
                            && event.created_at.gt(relay_list_timestamp))
                        || (KIND_USER_GRASP_LIST.eq(&event.kind)
                            && event.created_at.gt(grasp_list_timestamp))
                    {
                        report.profile_updates.insert(event.pubkey);
                    }
                }
                save_event_in_global_cache(git_repo_path, event).await?;
            }
        }
    }
    for event in &events {
        if !request.existing_events.contains(&event.id) {
            let tagged_root_id = event.tags.iter().find_map(|t| {
                if t.as_slice().len() > 1 && (t.as_slice()[0].eq("E") || t.as_slice()[0].eq("e")) {
                    EventId::parse(&t.as_slice()[1]).ok()
                } else {
                    None
                }
            });
            if status_kinds().contains(&event.kind) {
                // Route status events to the correct counter based on whether
                // the root event is a known issue or a proposal (patch/PR).
                // Don't double-count statuses that arrived in the same batch
                // as their parent (new issues/proposals already inflate the count).
                if let Some(root_id) = &tagged_root_id {
                    if report.issues.contains(root_id) {
                        // status for a new issue in this batch — skip (counted
                        // via issues)
                    } else if report.proposals.contains(root_id) {
                        // status for a new proposal in this batch — skip
                        // (counted via proposals)
                    } else if request.issue_ids.contains(root_id) {
                        report.issue_statuses.insert(event.id);
                    } else {
                        report.statuses.insert(event.id);
                    }
                }
            } else {
                // Non-status events: commits/PR-updates for proposals only.
                let not_tagged_with_new_proposal = tagged_root_id
                    .as_ref()
                    .is_none_or(|id| !report.proposals.contains(id));
                if not_tagged_with_new_proposal
                    && ((event.kind.eq(&Kind::GitPatch) && !event_is_patch_set_root(event))
                        || event.kind.eq(&KIND_PULL_REQUEST_UPDATE))
                {
                    report.commits.insert(event.id);
                }
            }
        }
    }
    expand_role_discovery(
        session_announcements,
        maintainer_listed,
        request,
        report,
        fresh_coordinates,
        fresh_profiles,
    );
    Ok(())
}

/// A fetched announcement's role listings, retained for the whole relay
/// fetch session so discovery expansion does not depend on the order in
/// which announcements arrive.
struct AnnouncementListing {
    author: PublicKey,
    identifier: String,
    maintainers: Vec<PublicKey>,
    moderators: Vec<PublicKey>,
}

/// Expand announcement discovery from fetched role listings.
///
/// Per NIP-34 clients SHOULD recursively fetch announcements from each
/// pubkey assigned a role, and only `M`/`m` members can assign: an
/// announcement expands discovery only when its author is themselves
/// maintainer-listed. A role-fetched announcement (e.g. a moderator's) is
/// consulted solely for its author's own self-entries during consolidation,
/// so its listings must not add coordinates or profiles — otherwise any
/// `o`-assigned pubkey could direct the client to fetch arbitrary
/// announcements. Fetching grants nothing — which assignments carry
/// authority is decided when the cache is consolidated
/// (`get_repo_ref_from_cache`).
///
/// Runs to a fixpoint over every announcement seen this session, so a
/// pubkey that joins the maintainer listing only after their own
/// announcement was processed still has that announcement's listings
/// expanded.
fn expand_role_discovery(
    announcements: &[AnnouncementListing],
    maintainer_listed: &mut HashSet<PublicKey>,
    request: &FetchRequest,
    report: &mut FetchReport,
    fresh_coordinates: &mut HashSet<Nip19Coordinate>,
    fresh_profiles: &mut HashSet<PublicKey>,
) {
    loop {
        let mut changed = false;
        for announcement in announcements {
            if !maintainer_listed.contains(&announcement.author) {
                continue;
            }
            for m in &announcement.maintainers {
                if maintainer_listed.insert(*m) {
                    changed = true;
                }
            }
            for m in announcement
                .maintainers
                .iter()
                .chain(announcement.moderators.iter())
            {
                if !request
                    .repo_coordinates_without_relays // prexisting members
                    .iter()
                    .map(|(c, _)| c.clone())
                    .collect::<HashSet<Nip19Coordinate>>()
                    .union(&report.repo_coordinates_without_relays) // already added members
                    .any(|c| c.identifier.eq(&announcement.identifier) && m.eq(&c.public_key))
                {
                    let c = Nip19Coordinate {
                        coordinate: Coordinate {
                            kind: Kind::GitRepoAnnouncement,
                            public_key: *m,
                            identifier: announcement.identifier.clone(),
                        },
                        relays: vec![],
                    };
                    fresh_coordinates.insert(c.clone());
                    report.repo_coordinates_without_relays.insert(c);

                    if !request.contributors.contains(m)
                        && !request
                            .profiles_to_fetch_from_user_relays
                            .clone()
                            .into_keys()
                            .collect::<HashSet<PublicKey>>()
                            .contains(m)
                        && !fresh_profiles.contains(m)
                    {
                        fresh_profiles.insert(m.to_owned());
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
}

fn request_includes_ngit_repo(request: &FetchRequest) -> bool {
    let coordinate = version_check::ngit_repo_coordinate();
    request
        .repo_coordinates_without_relays
        .iter()
        .any(|(c, _)| {
            c.identifier == coordinate.identifier && c.public_key == coordinate.public_key
        })
}

pub fn consolidate_fetch_reports(reports: Vec<Result<FetchReport>>) -> FetchReport {
    let mut report = FetchReport::default();
    for relay_report in reports.into_iter().flatten() {
        for c in relay_report.repo_coordinates_without_relays {
            if !report
                .repo_coordinates_without_relays
                .iter()
                .any(|e| e.eq(&c))
            {
                report.repo_coordinates_without_relays.insert(c);
            }
        }
        for (r, t) in relay_report.updated_repo_announcements {
            if let Some(i) = report
                .updated_repo_announcements
                .iter()
                .position(|(e, _)| e.eq(&r))
            {
                let (_, existing_t) = &report.updated_repo_announcements[i];
                if t.gt(existing_t) {
                    report.updated_repo_announcements[i] = (r, t);
                }
            } else {
                report.updated_repo_announcements.push((r, t));
            }
        }
        if let Some((timestamp, id)) = relay_report.updated_state {
            if let Some((existing_timestamp, existing_id)) = report.updated_state {
                if timestamp.gt(&existing_timestamp)
                    || (timestamp.eq(&existing_timestamp) && id.gt(&existing_id))
                {
                    report.updated_state = Some((timestamp, id));
                }
            } else {
                report.updated_state = Some((timestamp, id));
            }
        }
        for c in relay_report.proposals {
            report.proposals.insert(c);
        }
        for c in relay_report.commits {
            report.commits.insert(c);
        }
        for c in relay_report.statuses {
            report.statuses.insert(c);
        }
        for c in relay_report.issues {
            report.issues.insert(c);
        }
        for c in relay_report.issue_statuses {
            report.issue_statuses.insert(c);
        }
        for c in relay_report.comments {
            report.comments.insert(c);
        }
        for c in relay_report.labels {
            report.labels.insert(c);
        }
        for c in relay_report.cover_notes {
            report.cover_notes.insert(c);
        }
        report.deletions += relay_report.deletions;
        for c in relay_report.contributor_profiles {
            report.contributor_profiles.insert(c);
        }
        for c in relay_report.profile_updates {
            report.profile_updates.insert(c);
        }
        // Per-relay state events are independent: each relay entry is kept as-is.
        // If a relay appears in multiple per-relay reports (shouldn't happen in
        // practice but possible in tests), keep the newer event.
        for (relay_url, maybe_event) in relay_report.state_per_relay {
            match report.state_per_relay.entry(relay_url) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(maybe_event);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let keep = match (e.get(), &maybe_event) {
                        (None, Some(_)) => true,
                        (Some(existing), Some(incoming)) => {
                            incoming.created_at.gt(&existing.created_at)
                                || (incoming.created_at.eq(&existing.created_at)
                                    && incoming.id.gt(&existing.id))
                        }
                        _ => false,
                    };
                    if keep {
                        e.insert(maybe_event);
                    }
                }
            }
        }
    }
    report
}

/// A consolidated relay fetch that retains whether every relay completed.
///
/// An empty report after EOSE is materially different from an empty report
/// caused by a relay error during private repository discovery.
pub struct FetchOutcome {
    pub report: FetchReport,
    pub had_errors: bool,
    pub relay_count: usize,
}

pub fn consolidate_fetch_outcome(reports: Vec<Result<FetchReport>>) -> FetchOutcome {
    let had_errors = reports.iter().any(Result::is_err);
    let relay_count = reports.len();
    FetchOutcome {
        report: consolidate_fetch_reports(reports),
        had_errors,
        relay_count,
    }
}

impl FetchOutcome {
    pub fn all_required_relays_completed(&self, required_relay_count: usize) -> bool {
        required_relay_count > 0 && self.relay_count >= required_relay_count && !self.had_errors
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateRelayProbeDecision {
    UsePrivateResult,
    RetryPublicDiscovery,
    FailClosed,
}

/// Decide whether a query limited to kind-10318 relay hints may be broadened.
///
/// A private announcement keeps the result restricted even if another
/// repository relay failed. Otherwise every repository relay must have
/// completed successfully before ordinary public discovery is safe.
pub fn private_relay_probe_decision(
    discovered_privacy: Option<bool>,
    all_required_relays_completed: bool,
) -> PrivateRelayProbeDecision {
    if discovered_privacy == Some(true) {
        PrivateRelayProbeDecision::UsePrivateResult
    } else if all_required_relays_completed {
        PrivateRelayProbeDecision::RetryPublicDiscovery
    } else {
        PrivateRelayProbeDecision::FailClosed
    }
}

/// Whether resolving a repository still requires the selected account's
/// encrypted kind-10318 relay list.
///
/// A cached announcement already supplies the repository relays and a saved
/// private classification is required before asking the signer to decrypt.
/// An unresolved URL alone is not evidence of a private repository. Direct
/// NIP-11 private relay hints make account-wide discovery unnecessary.
pub fn needs_private_relay_discovery(
    configured_privacy: Option<bool>,
    has_cached_announcement: bool,
    has_nip11_private_relays: bool,
) -> bool {
    !has_cached_announcement && configured_privacy == Some(true) && !has_nip11_private_relays
}

pub fn get_fetch_filters(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    proposal_ids: &HashSet<EventId>,
    issue_ids: &HashSet<EventId>,
    non_proposal_event_ids: &HashSet<EventId>,
    required_profiles: &HashSet<PublicKey>,
) -> Vec<nostr::prelude::Filter> {
    [
        if repo_coordinates.is_empty() {
            vec![]
        } else {
            vec![
                get_filter_state_events(repo_coordinates, false),
                get_filter_repo_ann_events(repo_coordinates, false),
                nostr::prelude::Filter::default()
                    .kinds(vec![
                        Kind::GitPatch,
                        Kind::EventDeletion,
                        KIND_PULL_REQUEST,
                        Kind::GitIssue,
                    ])
                    .custom_tags(
                        SingleLetterTag::LOWERCASE_A,
                        repo_coordinates
                            .iter()
                            .map(|c| c.coordinate.to_string())
                            .collect::<Vec<String>>(),
                    ),
                get_filter_ci_events(repo_coordinates),
            ]
        },
        if proposal_ids.is_empty() {
            vec![]
        } else {
            vec![
                nostr::prelude::Filter::default()
                    .events(proposal_ids.clone())
                    .kinds(
                        [
                            vec![
                                Kind::GitPatch,
                                Kind::EventDeletion,
                                KIND_PULL_REQUEST_UPDATE,
                            ],
                            status_kinds(),
                        ]
                        .concat(),
                    ),
                nostr::prelude::Filter::default()
                    .custom_tags(SingleLetterTag::UPPERCASE_E, proposal_ids.clone())
                    .kinds(
                        [
                            vec![Kind::EventDeletion, KIND_PULL_REQUEST_UPDATE],
                            status_kinds(),
                        ]
                        .concat(),
                    ),
            ]
        },
        // Fetch status events for known issues.
        if issue_ids.is_empty() {
            vec![]
        } else {
            vec![
                nostr::prelude::Filter::default()
                    .events(issue_ids.clone())
                    .kinds(status_kinds()),
                nostr::prelude::Filter::default()
                    .custom_tags(SingleLetterTag::UPPERCASE_E, issue_ids.clone())
                    .kinds(status_kinds()),
            ]
        },
        // Fetch NIP-22 kind-1111 comments for issues and proposals (patches/PRs).
        // Comments use an uppercase `E` tag pointing to the root event ID.
        {
            let all_root_ids: HashSet<EventId> = issue_ids
                .iter()
                .chain(proposal_ids.iter())
                .copied()
                .collect();
            if all_root_ids.is_empty() {
                vec![]
            } else {
                vec![
                    nostr::prelude::Filter::default()
                        .custom_tags(SingleLetterTag::UPPERCASE_E, all_root_ids)
                        .kind(KIND_COMMENT),
                ]
            }
        },
        // Fetch NIP-32 kind-1985 label events for issues and proposals.
        // Label events reference the target via a lowercase `e` tag.
        {
            let all_root_ids: HashSet<EventId> = issue_ids
                .iter()
                .chain(proposal_ids.iter())
                .copied()
                .collect();
            if all_root_ids.is_empty() {
                vec![]
            } else {
                vec![
                    nostr::prelude::Filter::default()
                        .events(all_root_ids)
                        .kind(KIND_LABEL),
                ]
            }
        },
        // Fetch kind-1624 cover note events for issues and proposals.
        // Cover notes reference the target via a lowercase `e` tag.
        {
            let all_root_ids: HashSet<EventId> = issue_ids
                .iter()
                .chain(proposal_ids.iter())
                .copied()
                .collect();
            if all_root_ids.is_empty() {
                vec![]
            } else {
                vec![
                    nostr::prelude::Filter::default()
                        .events(all_root_ids)
                        .kind(KIND_COVER_NOTE),
                ]
            }
        },
        // Request kind-5 deletions for state events and repo announcements by
        // their event ID (#e tag), as per NIP-09. The #a-tagged filter above
        // covers addressable-event deletions; this covers the specific event IDs
        // of the state and announcement events we already have cached.
        if non_proposal_event_ids.is_empty() {
            vec![]
        } else {
            vec![
                nostr::prelude::Filter::default()
                    .kind(Kind::EventDeletion)
                    .events(non_proposal_event_ids.clone()),
            ]
        },
        if required_profiles.is_empty() {
            vec![]
        } else {
            vec![get_filter_contributor_profiles(required_profiles.clone())]
        },
    ]
    .concat()
}

fn get_announcement_only_fetch_filters(
    repo_coordinates: &HashSet<Nip19Coordinate>,
) -> Vec<nostr::prelude::Filter> {
    if repo_coordinates.is_empty() {
        vec![]
    } else {
        vec![get_filter_repo_ann_events(repo_coordinates, true)]
    }
}

fn get_auxiliary_fetch_filters(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    required_profiles: &HashSet<PublicKey>,
    announcements: bool,
    profiles: bool,
) -> Vec<nostr::prelude::Filter> {
    let mut filters = Vec::new();
    if announcements {
        filters.extend(get_announcement_only_fetch_filters(repo_coordinates));
    }
    if profiles && !required_profiles.is_empty() {
        filters.push(get_filter_contributor_profiles(required_profiles.clone()));
    }
    filters
}

pub fn get_filter_repo_ann_events(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    maintainers_only: bool,
) -> nostr::prelude::Filter {
    let filter = nostr::prelude::Filter::default()
        .kind(Kind::GitRepoAnnouncement)
        .identifiers(
            repo_coordinates
                .iter()
                .map(|c| c.identifier.clone())
                .collect::<Vec<String>>(),
        );
    if maintainers_only {
        filter.authors(
            repo_coordinates
                .iter()
                .map(|c| c.coordinate.public_key)
                .collect::<Vec<PublicKey>>(),
        )
    } else {
        filter
    }
}

/// Every CI event the repository's announcements are named on.
///
/// The consumed CI kinds all carry the repository `a` tag, so one
/// repository-wide filter brings Workflow Results, Progress markers, Job
/// Results, Service Requests/Stops and Manual Triggers into the local cache
/// during the fetch every PR command already performs. `ngit ci status`,
/// and later the `pr` surfaces, then read them from the cache.
pub fn get_filter_ci_events(repo_coordinates: &HashSet<Nip19Coordinate>) -> nostr::prelude::Filter {
    nostr::prelude::Filter::default()
        .kinds(crate::ci::kinds::CONSUMED_CI_KINDS.to_vec())
        .custom_tags(
            SingleLetterTag::LOWERCASE_A,
            repo_coordinates
                .iter()
                .map(|c| c.coordinate.to_string())
                .collect::<Vec<String>>(),
        )
}

pub static STATE_KIND: nostr::prelude::Kind = Kind::Custom(30618);
pub fn get_filter_state_events(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    maintainers_only: bool,
) -> nostr::prelude::Filter {
    let filter = nostr::prelude::Filter::default()
        .kind(STATE_KIND)
        .identifiers(
            repo_coordinates
                .iter()
                .map(|c| c.identifier.clone())
                .collect::<Vec<String>>(),
        );
    if maintainers_only {
        filter.authors(
            repo_coordinates
                .iter()
                .map(|c| c.coordinate.public_key)
                .collect::<Vec<PublicKey>>(),
        )
    } else {
        filter
    }
}

pub fn get_filter_contributor_profiles(contributors: HashSet<PublicKey>) -> nostr::prelude::Filter {
    nostr::prelude::Filter::default()
        .kinds(vec![Kind::Metadata, Kind::RelayList, KIND_USER_GRASP_LIST])
        .authors(contributors)
}

#[derive(Default)]
pub struct FetchReport {
    repo_coordinates_without_relays: HashSet<Nip19Coordinate>,
    updated_repo_announcements: Vec<(Nip19Coordinate, Timestamp)>,
    updated_state: Option<(Timestamp, EventId)>,
    proposals: HashSet<EventId>,
    /// commits against existing propoals
    commits: HashSet<EventId>,
    statuses: HashSet<EventId>,
    issues: HashSet<EventId>,
    issue_statuses: HashSet<EventId>,
    /// NIP-22 kind-1111 comments against issues, patches, and PRs.
    comments: HashSet<EventId>,
    /// NIP-32 kind-1985 label events for issues and proposals.
    labels: HashSet<EventId>,
    /// Kind-1624 cover note events for issues, patches, and PRs.
    cover_notes: HashSet<EventId>,
    /// Count of kind-5 deletion events received (for display purposes).
    deletions: u32,
    contributor_profiles: HashSet<PublicKey>,
    profile_updates: HashSet<PublicKey>,
    /// The best (newest) state event seen on each relay during the fetch.
    /// `None` as a value means the relay was queried but returned no state
    /// event at all.  Relays that were never queried are absent from the map.
    /// This is the only point at which per-relay state visibility is available;
    /// the local database only stores the canonical latest event.
    pub state_per_relay: HashMap<RelayUrl, Option<nostr::prelude::Event>>,
}

impl Display for FetchReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // report: "1 announcement, 1 announcement, 1 proposal, 3 commits, 2
        // statuses"
        let mut display_items: Vec<String> = vec![];
        if !self.repo_coordinates_without_relays.is_empty() {
            display_items.push(format!(
                "{} announcement{}",
                self.repo_coordinates_without_relays.len(),
                if self.repo_coordinates_without_relays.len() > 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        if !self.updated_repo_announcements.is_empty() {
            display_items.push(format!(
                "{} announcement update{}",
                self.updated_repo_announcements.len(),
                if self.updated_repo_announcements.len() > 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        if self.updated_state.is_some() {
            display_items.push("new state".to_string());
        }
        if !self.proposals.is_empty() {
            display_items.push(format!(
                "{} proposal{}",
                self.proposals.len(),
                if self.proposals.len() > 1 { "s" } else { "" },
            ));
        }
        if !self.commits.is_empty() {
            display_items.push(format!(
                "{} commit{}",
                self.commits.len(),
                if self.commits.len() > 1 { "s" } else { "" },
            ));
        }
        if !self.statuses.is_empty() {
            display_items.push(format!(
                "{} status{}",
                self.statuses.len(),
                if self.statuses.len() > 1 { "es" } else { "" },
            ));
        }
        if !self.issues.is_empty() {
            display_items.push(format!(
                "{} issue{}",
                self.issues.len(),
                if self.issues.len() > 1 { "s" } else { "" },
            ));
        }
        if !self.issue_statuses.is_empty() {
            display_items.push(format!(
                "{} issue status{}",
                self.issue_statuses.len(),
                if self.issue_statuses.len() > 1 {
                    "es"
                } else {
                    ""
                },
            ));
        }
        if !self.comments.is_empty() {
            display_items.push(format!(
                "{} comment{}",
                self.comments.len(),
                if self.comments.len() > 1 { "s" } else { "" },
            ));
        }
        if !self.labels.is_empty() {
            display_items.push(format!(
                "{} label{}",
                self.labels.len(),
                if self.labels.len() > 1 { "s" } else { "" },
            ));
        }
        if !self.cover_notes.is_empty() {
            display_items.push(format!(
                "{} cover note{}",
                self.cover_notes.len(),
                if self.cover_notes.len() > 1 { "s" } else { "" },
            ));
        }
        if self.deletions > 0 {
            display_items.push(format!(
                "{} deletion{}",
                self.deletions,
                if self.deletions > 1 { "s" } else { "" },
            ));
        }
        if !self.contributor_profiles.is_empty() {
            display_items.push(format!(
                "{} user profile{}",
                self.contributor_profiles.len(),
                if self.contributor_profiles.len() > 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        if !self.profile_updates.is_empty() {
            display_items.push(format!(
                "{} profile update{}",
                self.profile_updates.len(),
                if self.profile_updates.len() > 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        write!(f, "{}", display_items.join(", "))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
enum RelayFetchScope {
    /// Fetch repository announcements, state and collaboration events, plus
    /// any user data needed to render their authors.
    #[default]
    Repository,
    /// Fetch only the non-collaboration data explicitly assigned to this
    /// relay. A relay can be both an announcement indexer and a user relay.
    Auxiliary { announcements: bool, profiles: bool },
}

#[derive(Default, Clone)]
pub struct FetchRequest {
    repo_relays: HashSet<RelayUrl>,
    announcement_indexer_relays: HashSet<RelayUrl>,
    /// NIP-65 write relays that must complete announcement discovery rather
    /// than being curtailed by the adaptive success threshold.
    author_announcement_relays: HashSet<RelayUrl>,
    /// Authors whose relay lists must be bootstrapped from announcement
    /// indexers before their write relays can also locate an announcement.
    announcement_profile_authors: HashSet<PublicKey>,
    repository_relays_only: bool,
    lock_repository_relays: bool,
    selected_relay: Option<RelayUrl>,
    scope: RelayFetchScope,
    repo_auth_mode: RelayAuthMode,
    relay_column_width: usize,
    repo_coordinates_without_relays: Vec<(Nip19Coordinate, Option<Timestamp>)>,
    /// Authors whose announcements expand discovery: the selected
    /// maintainer plus every pubkey a maintainer-listed announcement lists
    /// as `M`/`m`. Per NIP-34 only members can assign roles, so a
    /// role-fetched (e.g. moderator-only) author's listings never expand
    /// discovery; grown per fetch session by `expand_role_discovery`.
    maintainer_listed_authors: HashSet<PublicKey>,
    state: Option<(Timestamp, EventId)>,
    proposals: HashSet<EventId>,
    /// Known issue event IDs, used to fetch their status events.
    issue_ids: HashSet<EventId>,
    /// Event IDs of non-proposal events (state events, repo announcements) for
    /// which we should also request kind-5 deletion events by `#e` tag.
    non_proposal_event_ids: HashSet<EventId>,
    contributors: HashSet<PublicKey>,
    missing_contributor_profiles: HashSet<PublicKey>,
    existing_events: HashSet<EventId>,
    profiles_to_fetch_from_user_relays: HashMap<PublicKey, (Timestamp, Timestamp, Timestamp)>,
    user_relays_for_profiles: HashSet<RelayUrl>,
    private_relay_list_authors: HashSet<PublicKey>,
}

impl FetchRequest {
    fn relay_processing_key(&self) -> Option<(RelayUrl, RelayFetchScope, bool)> {
        let relay = self.selected_relay.clone()?;
        let scope = match self.scope {
            RelayFetchScope::Repository => RelayFetchScope::Repository,
            RelayFetchScope::Auxiliary { announcements, .. } => {
                // Bootstrap profiles are fetched as part of an announcement
                // indexer query. Once the announcement resolves, clearing the
                // bootstrap author must not make the same indexer query look
                // new. Profile work independently assigned to this user's
                // relay remains part of the key so later scope upgrades run.
                RelayFetchScope::Auxiliary {
                    announcements,
                    profiles: self.user_relays_for_profiles.contains(&relay),
                }
            }
        };
        Some((relay, scope, self.fetches_private_relay_lists()))
    }

    fn complete_hintless_announcement_discovery(&mut self) {
        self.announcement_profile_authors.clear();
        self.author_announcement_relays.clear();
    }

    fn add_author_announcement_relays(&mut self, relays: impl IntoIterator<Item = RelayUrl>) {
        let relays = relays.into_iter().collect::<Vec<_>>();
        self.announcement_indexer_relays
            .extend(relays.iter().cloned());
        self.author_announcement_relays.extend(relays);
    }

    fn fetches_private_relay_lists(&self) -> bool {
        self.selected_relay.as_ref().is_some_and(|relay| {
            self.user_relays_for_profiles.contains(relay)
                && !self.private_relay_list_authors.is_empty()
        })
    }

    fn add_private_relay_list_filter(&self, filters: &mut Vec<nostr::prelude::Filter>) {
        if self.fetches_private_relay_lists() {
            filters.push(
                nostr::prelude::Filter::new()
                    .kind(KIND_PRIVATE_GIT_RELAY_LIST)
                    .authors(self.private_relay_list_authors.clone())
                    .limit(10),
            );
        }
    }

    /// Restrict a request to the data this relay is authoritative for.
    ///
    /// Repository relays receive the complete request. Other relays may be
    /// queried for maintainer announcements or user profile data, but never
    /// receive repository state, issue, proposal, comment, label, cover-note
    /// or deletion filters.
    fn scoped_to_relay(&self, relay: &RelayUrl) -> Self {
        let mut scoped = self.clone();
        scoped.selected_relay = Some(relay.clone());
        if self.repo_relays.contains(relay) {
            scoped.scope = RelayFetchScope::Repository;
            return scoped;
        }

        let announcements = self.announcement_indexer_relays.contains(relay);
        let user_profiles = self.user_relays_for_profiles.contains(relay);
        let bootstrap_profiles = announcements && !self.announcement_profile_authors.is_empty();
        let profiles = user_profiles || bootstrap_profiles;
        scoped.scope = RelayFetchScope::Auxiliary {
            announcements,
            profiles,
        };
        if !announcements {
            scoped.repo_coordinates_without_relays.clear();
        }
        scoped.state = None;
        scoped.proposals.clear();
        scoped.issue_ids.clear();
        scoped.non_proposal_event_ids.clear();
        if profiles {
            scoped.missing_contributor_profiles = if user_profiles {
                self.missing_contributor_profiles
                    .union(
                        &self
                            .profiles_to_fetch_from_user_relays
                            .clone()
                            .into_keys()
                            .collect(),
                    )
                    .copied()
                    .collect()
            } else {
                self.announcement_profile_authors.clone()
            };
            scoped
                .profiles_to_fetch_from_user_relays
                .retain(|author, _| scoped.missing_contributor_profiles.contains(author));
        } else {
            scoped.missing_contributor_profiles.clear();
            scoped.profiles_to_fetch_from_user_relays.clear();
        }
        scoped
    }
}

pub async fn fetching_with_report(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
) -> Result<FetchReport> {
    fetching_with_report_policy(git_repo_path, client, selected_maintainer_coordinate, false).await
}

/// Fetch repository data without sending repository coordinates to configured
/// indexers or fallback relays.
pub async fn fetching_with_report_from_repository_relays(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
) -> Result<FetchReport> {
    fetching_with_report_policy(git_repo_path, client, selected_maintainer_coordinate, true).await
}

async fn fetching_with_report_from_repository_relays_with_outcome(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
) -> Result<FetchOutcome> {
    fetching_with_report_policy_outcome(
        git_repo_path,
        client,
        selected_maintainer_coordinate,
        true,
        true,
    )
    .await
}

/// Fetch with private discovery hints, probing only those repository relays
/// until repository privacy is known.
///
/// Once a cached or freshly fetched announcement marks the repository private,
/// the coordinate is never sent to configured announcement indexers or
/// fallback relays.
pub async fn fetching_with_private_discovery(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    coordinate: &mut Nip19Coordinate,
    private_discovery: &PrivateGitRelayDiscovery,
) -> Result<FetchReport> {
    if let Some(repo_ref) = get_repo_ref_from_cache(Some(git_repo_path), coordinate)
        .await
        .ok()
        .filter(|repo_ref| repo_ref.private)
    {
        save_repository_privacy_to_git_config(git_repo_path, true);
        coordinate
            .relays
            .retain(|relay| repo_ref.relays.contains(relay));
        for relay in repo_ref.relays {
            if !coordinate.relays.contains(&relay) {
                coordinate.relays.push(relay);
            }
        }
        return fetching_with_report_from_repository_relays(git_repo_path, client, coordinate)
            .await;
    }

    if let PrivateGitRelayDiscovery::Unavailable(error) = private_discovery {
        if get_repo_ref_from_cache(Some(git_repo_path), coordinate)
            .await
            .is_ok()
        {
            return fetching_with_report_from_repository_relays(git_repo_path, client, coordinate)
                .await;
        }
        bail!("private Git relay discovery is unavailable: {error}");
    }

    if private_discovery.requires_repository_only_probe() {
        let mut private_coordinate = coordinate.clone();
        private_coordinate.relays = private_discovery.relays().to_vec();
        let private_outcome = fetching_with_report_from_repository_relays_with_outcome(
            git_repo_path,
            client,
            &private_coordinate,
        )
        .await?;
        let discovered_repo_ref = get_repo_ref_from_cache(Some(git_repo_path), coordinate)
            .await
            .ok();
        let discovered_privacy = discovered_repo_ref
            .as_ref()
            .map(|repo_ref| repo_ref.private);
        let private_probe_completed =
            private_outcome.all_required_relays_completed(private_discovery.relays().len());
        match private_relay_probe_decision(discovered_privacy, private_probe_completed) {
            PrivateRelayProbeDecision::UsePrivateResult => {
                if let Some(repo_ref) = discovered_repo_ref {
                    save_repository_privacy_to_git_config(git_repo_path, repo_ref.private);
                    coordinate.relays = repo_ref.relays;
                }
                return Ok(private_outcome.report);
            }
            PrivateRelayProbeDecision::RetryPublicDiscovery => {}
            PrivateRelayProbeDecision::FailClosed => {
                bail!(
                    "private repository relay probe failed; refusing to query public discovery relays"
                );
            }
        }
    }

    let report = fetching_with_report(git_repo_path, client, coordinate).await?;
    if let Ok(repo_ref) = get_repo_ref_from_cache(Some(git_repo_path), coordinate).await {
        save_repository_privacy_to_git_config(git_repo_path, repo_ref.private);
    }
    Ok(report)
}

async fn fetching_with_report_policy(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
    repository_relays_only: bool,
) -> Result<FetchReport> {
    Ok(fetching_with_report_policy_outcome(
        git_repo_path,
        client,
        selected_maintainer_coordinate,
        repository_relays_only,
        true,
    )
    .await?
    .report)
}

/// Fetch repository data while leaving the terminal summary to the caller.
///
/// Relay progress and diagnostics are still completed normally. This is for
/// internal consistency refreshes within a command which already reported its
/// initial fetch to the user.
pub async fn fetching_without_summary(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
) -> Result<FetchReport> {
    Ok(fetching_with_report_policy_outcome(
        git_repo_path,
        client,
        selected_maintainer_coordinate,
        false,
        false,
    )
    .await?
    .report)
}

async fn fetching_with_report_policy_outcome(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
    repository_relays_only: bool,
    print_summary: bool,
) -> Result<FetchOutcome> {
    let (relay_reports, progress_reporter) = client
        .fetch_all(
            Some(git_repo_path),
            Some(selected_maintainer_coordinate),
            &HashSet::new(),
            &HashSet::new(),
            repository_relays_only,
        )
        .await?;
    finish_fetch_progress(&relay_reports, progress_reporter)?;
    let outcome = consolidate_fetch_outcome(relay_reports);
    // Route the summary to stderr so stdout stays clean for JSON-emitting
    // subcommands (e.g. `ngit issue list --json | jq .`). The progress bars
    // above also write to stderr, keeping all human-facing fetch chatter off
    // stdout.
    if print_summary {
        let term = console::Term::stderr();
        if outcome.report.to_string().is_empty() {
            write_progress_line(&term, "no updates")?;
        } else {
            write_progress_line(&term, &format!("updates: {}", outcome.report))?;
        }
    }
    Ok(outcome)
}

/// Like `fetching_with_report` but suppresses the "no updates" / "updates: X"
/// summary line. Returns `true` if any relay reported an error (so the caller
/// can print a blank line to visually separate relay-error output from
/// subsequent content).
pub async fn fetching_quietly(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    selected_maintainer_coordinate: &Nip19Coordinate,
    private_discovery: &PrivateGitRelayDiscovery,
) -> Result<(FetchReport, bool)> {
    let cached_repo_ref =
        get_repo_ref_from_cache(Some(git_repo_path), selected_maintainer_coordinate)
            .await
            .ok();
    if let PrivateGitRelayDiscovery::Unavailable(error) = private_discovery {
        if cached_repo_ref.is_none() {
            bail!("private Git relay discovery is unavailable: {error}");
        }
    }
    let repository_relays_only = private_discovery.requires_repository_only_probe()
        || cached_repo_ref.is_some_and(|repo_ref| repo_ref.private)
        || matches!(private_discovery, PrivateGitRelayDiscovery::Unavailable(_));
    let (relay_reports, progress_reporter) = client
        .fetch_all(
            Some(git_repo_path),
            Some(selected_maintainer_coordinate),
            &HashSet::new(),
            &HashSet::new(),
            repository_relays_only,
        )
        .await?;
    let had_errors = finish_fetch_progress(&relay_reports, progress_reporter)?;
    let report = consolidate_fetch_reports(relay_reports);
    Ok((report, had_errors))
}

/// Finalize a repository fetch before its caller prints ordinary output.
///
/// Partial auxiliary or repository relay outages are routine and remain
/// transient. Diagnostics are retained only when every attempted relay failed
/// or no repository relay completed successfully.
pub fn finish_fetch_progress<T>(
    reports: &[Result<T>],
    progress_reporter: RelayProgressReporter,
) -> Result<bool> {
    let had_errors = reports.iter().any(Result::is_err);
    let successful_relays = reports.iter().filter(|report| report.is_ok()).count();
    let show_diagnostics = fetch_failure_requires_diagnostics(
        reports.len(),
        successful_relays,
        progress_reporter
            .fetch_relay_health
            .repository_relay_attempts,
        progress_reporter
            .fetch_relay_health
            .repository_relay_successes,
    );
    progress_reporter.finish(had_errors, show_diagnostics, None)?;
    Ok(had_errors)
}

fn fetch_failure_requires_diagnostics(
    relay_attempts: usize,
    relay_successes: usize,
    repository_relay_attempts: usize,
    repository_relay_successes: usize,
) -> bool {
    let all_relays_failed = relay_attempts > 0 && relay_successes == 0;
    let all_repository_relays_failed =
        repository_relay_attempts > 0 && repository_relay_successes == 0;
    all_relays_failed || all_repository_relays_failed
}

pub async fn get_issues_from_cache(
    git_repo_path: &Path,
    repo_coordinates: HashSet<Nip19Coordinate>,
) -> Result<Vec<nostr::prelude::Event>> {
    let mut issues = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .kinds([nostr::prelude::Kind::GitIssue])
                .custom_tags(
                    nostr::prelude::SingleLetterTag::LOWERCASE_A,
                    repo_coordinates
                        .iter()
                        .map(|c| c.coordinate.to_string())
                        .collect::<Vec<String>>(),
                ),
        ],
    )
    .await?;
    issues.sort_by_key(|e| e.created_at);
    issues.reverse();
    Ok(issues)
}

pub async fn get_proposals_and_revisions_from_cache(
    git_repo_path: &Path,
    repo_coordinates: HashSet<Nip19Coordinate>,
) -> Result<Vec<nostr::prelude::Event>> {
    let mut proposals = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .kinds([nostr::prelude::Kind::GitPatch, KIND_PULL_REQUEST])
                .custom_tags(
                    nostr::prelude::SingleLetterTag::LOWERCASE_A,
                    repo_coordinates
                        .iter()
                        .map(|c| c.coordinate.to_string())
                        .collect::<Vec<String>>(),
                ),
        ],
    )
    .await?
    .iter()
    .filter(|e| event_is_patch_set_root(e) || e.kind.eq(&KIND_PULL_REQUEST))
    .filter(|e| e.kind.eq(&Kind::GitPatch) || event_is_valid_pr_or_pr_update(e))
    .cloned()
    .collect::<Vec<nostr::prelude::Event>>();
    proposals.sort_by_key(|e| e.created_at);
    proposals.reverse();
    Ok(proposals)
}

pub async fn get_all_proposal_patch_pr_pr_update_events_from_cache(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    proposal_id: &nostr::prelude::EventId,
) -> Result<Vec<nostr::prelude::Event>> {
    let mut commit_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .kinds([
                    nostr::prelude::Kind::GitPatch,
                    KIND_PULL_REQUEST,
                    KIND_PULL_REQUEST_UPDATE,
                ])
                .event(*proposal_id),
            nostr::prelude::Filter::default()
                .kinds([
                    nostr::prelude::Kind::GitPatch,
                    KIND_PULL_REQUEST,
                    KIND_PULL_REQUEST_UPDATE,
                ])
                .custom_tag(SingleLetterTag::UPPERCASE_E, *proposal_id),
            nostr::prelude::Filter::default()
                .kinds([nostr::prelude::Kind::GitPatch, KIND_PULL_REQUEST])
                .id(*proposal_id),
        ],
    )
    .await?;

    let permissioned_users: HashSet<PublicKey> = [
        repo_ref.maintainers.clone(),
        vec![
            commit_events
                .iter()
                .find(|e| e.id.eq(proposal_id))
                .context("proposal not in cache")?
                .pubkey,
        ],
    ]
    .concat()
    .iter()
    .copied()
    .collect();

    commit_events.retain(|e| {
        permissioned_users.contains(&e.pubkey)
            && (e.kind.eq(&Kind::GitPatch) || event_is_valid_pr_or_pr_update(e))
    });

    let revision_roots: HashSet<nostr::prelude::EventId> = commit_events
        .iter()
        .filter(|e| event_is_revision_root(e))
        .map(|e| e.id)
        .collect();

    if !revision_roots.is_empty() {
        for event in get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .kinds([
                        nostr::prelude::Kind::GitPatch,
                        KIND_PULL_REQUEST,
                        KIND_PULL_REQUEST_UPDATE,
                    ])
                    .events(revision_roots.clone())
                    .authors(permissioned_users.clone()),
                nostr::prelude::Filter::default()
                    .kinds([
                        nostr::prelude::Kind::GitPatch,
                        KIND_PULL_REQUEST,
                        KIND_PULL_REQUEST_UPDATE,
                    ])
                    .custom_tags(SingleLetterTag::UPPERCASE_E, revision_roots)
                    .authors(permissioned_users.clone()),
            ],
        )
        .await?
        {
            commit_events.push(event);
        }
    }

    Ok(commit_events
        .iter()
        .filter(|e| !event_is_cover_letter(e) && permissioned_users.contains(&e.pubkey))
        .cloned()
        .collect())
}

pub async fn get_event_from_cache_by_id(git_repo: &Repo, event_id: &EventId) -> Result<Event> {
    Ok(get_events_from_local_cache(
        git_repo.get_path()?,
        vec![nostr::prelude::Filter::default().id(*event_id)],
    )
    .await?
    .first()
    .context("failed to find event in cache")?
    .clone())
}

/// Whether a relay send failure is an `OK: false` response whose
/// machine-readable prefix is `duplicate:` (NIP-01): the relay refused
/// the resend because it already holds the event. Callers treat this as
/// successful delivery — most importantly the GRASP staging gate, which
/// must not skip a git server whose paired relay demonstrably holds the
/// current state event.
fn event_rejection_is_duplicate(error: &NostrSdkError) -> bool {
    error.kind() == NostrSdkErrorKind::Rejected
        && matches!(
            MachineReadablePrefix::parse(&error.to_string()),
            Some(MachineReadablePrefix::Duplicate)
        )
}

#[allow(clippy::module_name_repetitions)]
/// Per-relay outcome of one publication attempt.
///
/// The relay URL is reported without a trailing slash, matching the keys
/// returned by [`send_events`]. `error` carries the relay's rejection or
/// transport error so callers can report it even when the interactive
/// progress display is hidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayPublishOutcome {
    pub relay: String,
    pub error: Option<String>,
}

impl RelayPublishOutcome {
    #[must_use]
    pub fn accepted(relay: impl Into<String>) -> Self {
        Self {
            relay: relay.into(),
            error: None,
        }
    }

    #[must_use]
    pub fn rejected(relay: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            relay: relay.into(),
            error: Some(error.into()),
        }
    }

    #[must_use]
    pub fn accepted_by_relay(&self) -> bool {
        self.error.is_none()
    }

    fn into_result(self) -> (String, bool) {
        let accepted = self.accepted_by_relay();
        (self.relay, accepted)
    }
}

fn outcomes_to_results(outcomes: Vec<RelayPublishOutcome>) -> Vec<(String, bool)> {
    outcomes
        .into_iter()
        .map(RelayPublishOutcome::into_result)
        .collect()
}

fn require_publication_acceptance(outcomes: &[RelayPublishOutcome]) -> Result<()> {
    if outcomes.iter().any(RelayPublishOutcome::accepted_by_relay) {
        return Ok(());
    }
    if outcomes.is_empty() {
        bail!("failed to publish events: no publication relays configured");
    }
    let failures = outcomes
        .iter()
        .map(|outcome| {
            format!(
                "{}: {}",
                outcome.relay,
                outcome.error.as_deref().unwrap_or("unknown error")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    bail!("failed to publish events to any relay: {failures}")
}

/// Publish events, requiring at least one relay to accept the entire batch.
/// An empty batch is a no-op. Partial relay failure remains successful when
/// another relay accepts every event. Errors include the relay failure reasons.
pub async fn send_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: Option<&Path>,
    events: Vec<nostr::prelude::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<Vec<(String, bool)>> {
    if events.is_empty() {
        return Ok(vec![]);
    }
    let outcomes = send_events_with_cache_path(
        client,
        git_repo_path,
        git_repo_path,
        true,
        events,
        my_write_relays,
        repo_read_relays,
        animate,
        silent,
    )
    .await?;
    require_publication_acceptance(&outcomes)?;
    Ok(outcomes_to_results(outcomes))
}

/// Publish events and return per-relay results even when every relay fails.
/// Callers must check acceptance before reporting success. Use this for
/// transaction decisions or domain-specific error and recovery reports;
/// ordinary commands should use [`send_events`].
#[allow(clippy::module_name_repetitions)]
pub async fn send_events_with_results(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: Option<&Path>,
    events: Vec<nostr::prelude::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<Vec<(String, bool)>> {
    Ok(outcomes_to_results(
        send_events_with_cache_path(
            client,
            git_repo_path,
            git_repo_path,
            true,
            events,
            my_write_relays,
            repo_read_relays,
            animate,
            silent,
        )
        .await?,
    ))
}

/// Publish events without writing them into the repository's local event
/// cache on success. `git_repo_path` still drives repository
/// configuration lookups (`nostr.repo-relay-only`). Use this when an event must
/// not become locally authoritative until the caller explicitly commits
/// it — e.g. an unverified repository-state candidate. Like
/// [`send_events_with_results`], this returns outcomes even on total relay
/// failure so the transaction can combine them with earlier publication.
#[allow(clippy::module_name_repetitions)]
pub async fn send_events_without_caching(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: Option<&Path>,
    events: Vec<nostr::prelude::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<Vec<(String, bool)>> {
    Ok(outcomes_to_results(
        publish_events_without_caching(
            client,
            git_repo_path,
            events,
            my_write_relays,
            repo_read_relays,
            animate,
            silent,
        )
        .await?,
    ))
}

/// [`send_events_without_caching`] returning each relay's
/// [`RelayPublishOutcome`], including the rejection reason, instead of a
/// bare acceptance flag.
#[allow(clippy::module_name_repetitions)]
pub async fn publish_events_without_caching(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: Option<&Path>,
    events: Vec<nostr::prelude::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<Vec<RelayPublishOutcome>> {
    send_events_with_cache_path(
        client,
        None,
        git_repo_path,
        true,
        events,
        my_write_relays,
        repo_read_relays,
        animate,
        silent,
    )
    .await
}

/// Publish account-scoped public events without applying private-repository or
/// `nostr.repo-relay-only` routing from the current Git repository.
///
/// Successful events may still be cached in that repository when
/// `git_repo_path` is present; the path has no effect on relay selection.
#[allow(clippy::module_name_repetitions)]
pub async fn send_public_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: Option<&Path>,
    events: Vec<nostr::prelude::Event>,
    my_write_relays: Vec<String>,
    additional_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<Vec<(String, bool)>> {
    Ok(outcomes_to_results(
        send_events_with_cache_path(
            client,
            git_repo_path,
            None,
            false,
            events,
            my_write_relays,
            additional_relays,
            animate,
            silent,
        )
        .await?,
    ))
}

/// Shared implementation of [`send_events`] /
/// [`send_events_without_caching`]. `cache_path` controls whether
/// successfully-sent events are saved into the repository's local event
/// cache; `config_repo_path` locates the repository configuration
/// consulted for `nostr.repo-relay-only`.
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn send_events_with_cache_path(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    cache_path: Option<&Path>,
    config_repo_path: Option<&Path>,
    repository_scoped: bool,
    events: Vec<nostr::prelude::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<Vec<RelayPublishOutcome>> {
    let locally_private = repository_scoped
        && config_repo_path.is_some_and(|path| {
            git2::Repository::open(path)
                .ok()
                .and_then(|repo| repo.config().ok())
                .and_then(|config| config.get_bool("nostr.private").ok())
                .unwrap_or(false)
        });
    let private_repository = if repository_scoped {
        private_for_publication(config_repo_path, &events, locally_private).await
    } else {
        false
    };
    let repository_only_requested = std::env::var("NGIT_REPO_RELAY_ONLY").is_ok()
        || config_repo_path.is_some_and(|path| {
            git2::Repository::open(path)
                .ok()
                .and_then(|repo| repo.config().ok())
                .and_then(|config| config.get_bool("nostr.repo-relay-only").ok())
                .unwrap_or(false)
        });
    let repo_relay_only = repository_only_routing(
        repository_scoped,
        private_repository,
        repository_only_requested,
    );

    if repo_relay_only && repo_read_relays.is_empty() {
        bail!("repository-only publication requires at least one repository relay")
    }

    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        my_write_relays
    };

    // Only include default relays as fallback when there are no repo relays
    // (bootstrapping case, e.g. new account signup). When repo relays exist,
    // trust the repo and user relay configuration.
    let fallback = [
        if !repo_relay_only && repo_read_relays.is_empty() && my_write_relays.is_empty() {
            client.get_relay_default_set().clone()
        } else {
            vec![]
        },
        if !repo_relay_only && events.iter().any(|e| e.kind.eq(&Kind::GitRepoAnnouncement)) {
            client.get_blaster_relays().clone()
        } else {
            vec![]
        },
    ]
    .concat();
    let mut relays: Vec<&str> = vec![];

    if private_repository {
        client.nip42_register_private_repo_relays(repo_read_relays.clone());
    } else {
        client.nip42_register_repo_relays(repo_read_relays.clone());
    }
    client.nip42_register_publish_relays(
        my_write_relays
            .iter()
            .filter_map(|relay| RelayUrl::parse(relay).ok())
            .collect(),
    );

    let repo_read_relays = repo_read_relays
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<String>>();

    let all = &[
        repo_read_relays.clone(),
        my_write_relays.clone(),
        fallback.clone(),
    ]
    .concat();
    // add duplicates first
    for r in &repo_read_relays {
        let r_clean = remove_trailing_slash(r);
        if !my_write_relays
            .iter()
            .filter(|x| r_clean.eq(&remove_trailing_slash(x)))
            .count()
            > 1
            && !relays.iter().any(|x| r_clean.eq(&remove_trailing_slash(x)))
        {
            relays.push(r);
        }
    }

    for r in all {
        let r_clean = remove_trailing_slash(r);
        if !relays.iter().any(|x| r_clean.eq(&remove_trailing_slash(x))) {
            relays.push(r);
        }
    }

    let events_description = describe_events(&events);
    let progress_reporter = RelayProgressReporter::new(
        format!("Publishing {events_description} to nostr relays..."),
        animate,
        silent,
    );
    let progress = progress_reporter.handle();

    let pb_style = ProgressStyle::with_template(if animate {
        " {spinner} {prefix} {bar} {pos}/{len} {msg}"
    } else {
        " - {prefix} {bar} {pos}/{len} {msg}"
    })?
    .progress_chars("##-");

    let pb_after_style =
        |symbol| ProgressStyle::with_template(format!(" {symbol} {}", "{prefix} {msg}",).as_str());
    let pb_after_style_succeeded = pb_after_style(if animate {
        console::style("✔".to_string())
            .for_stderr()
            .green()
            .to_string()
    } else {
        "y".to_string()
    })?;

    let pb_after_style_failed = pb_after_style(if animate {
        console::style("✘".to_string())
            .for_stderr()
            .red()
            .to_string()
    } else {
        "x".to_string()
    })?;

    #[allow(clippy::borrow_deref_ref)]
    let relay_results: Vec<RelayPublishOutcome> = join_all(relays.iter().map(|&relay| {
        let progress = progress.clone();
        let my_write_relays = my_write_relays.clone();
        let repo_read_relays = repo_read_relays.clone();
        let fallback = fallback.clone();
        let events = events.clone();
        let pb_style = pb_style.clone();
        let pb_after_style_failed = pb_after_style_failed.clone();
        let pb_after_style_succeeded = pb_after_style_succeeded.clone();
        async move {
            let relay_clean = remove_trailing_slash(relay);
            let details = format!(
                "{}{}{} {}",
                if my_write_relays
                    .iter()
                    .any(|r| relay_clean.eq(&remove_trailing_slash(r)))
                {
                    " [my-relay]"
                } else {
                    ""
                },
                if repo_read_relays
                    .iter()
                    .any(|r| relay_clean.eq(&remove_trailing_slash(&r.to_string())))
                {
                    " [repo-relay]"
                } else {
                    ""
                },
                if fallback
                    .iter()
                    .any(|r| relay_clean.eq(&remove_trailing_slash(r)))
                {
                    " [default]"
                } else {
                    ""
                },
                relay_clean,
            );
            let pb = progress.add(
                ProgressBar::new(events.len() as u64)
                    .with_prefix(details.to_string())
                    .with_style(pb_style.clone()),
            );
            if animate {
                pb.enable_steady_tick(Duration::from_millis(300));
            }
            pb.inc(0); // need to make pb display intially
            let mut error = None;
            for event in &events {
                match client.send_event_to(cache_path, relay, event.clone()).await {
                    Ok(_) => pb.inc(1),
                    Err(e) => {
                        pb.set_style(pb_after_style_failed.clone());
                        let reason = e
                            .to_string()
                            .replace("relay pool error:", "")
                            .replace("event not published: ", "")
                            .trim()
                            .to_string();
                        let msg = console::style(format!("error: {reason}"))
                            .for_stderr()
                            .red()
                            .to_string();
                        progress.finish_bar(&pb, msg);
                        error = Some(reason);
                        break;
                    }
                };
            }
            if error.is_none() {
                pb.set_style(pb_after_style_succeeded.clone());
                progress.finish_bar(&pb, String::new());
            }
            RelayPublishOutcome {
                relay: relay_clean.to_string(),
                error,
            }
        }
    }))
    .await;

    let succeeded_count = relay_results
        .iter()
        .filter(|outcome| outcome.accepted_by_relay())
        .count();
    let total_count = relay_results.len();
    let failed_relays: Vec<&str> = relay_results
        .iter()
        .filter(|outcome| !outcome.accepted_by_relay())
        .map(|outcome| {
            let url = outcome.relay.as_str();
            url.strip_prefix("wss://")
                .or_else(|| url.strip_prefix("ws://"))
                .unwrap_or(url)
                .trim_end_matches('/')
        })
        .collect();

    let finish_message = if succeeded_count == total_count {
        format!("Published {events_description} to {total_count} relays")
    } else if succeeded_count > 0 {
        format!(
            "Published {events_description} to {succeeded_count}/{total_count} relays (failed: {})",
            failed_relays.join(" ")
        )
    } else {
        format!(
            "failed to publish {events_description} to any relay (failed: {})",
            failed_relays.join(" ")
        )
    };

    progress_reporter.finish(
        succeeded_count != total_count,
        total_count > 0 && succeeded_count == 0,
        Some(finish_message),
    )?;

    Ok(relay_results)
}

fn repository_only_routing(
    repository_scoped: bool,
    private_repository: bool,
    repository_only_requested: bool,
) -> bool {
    repository_scoped && (private_repository || repository_only_requested)
}

/// Builds a human-readable description of what is being published, e.g.
/// "3 patches", "1 announcement and 1 state event", "2 patches and 1 cover
/// letter".
fn describe_events(events: &[nostr::prelude::Event]) -> String {
    use crate::git_events::{KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, KIND_USER_GRASP_LIST};

    // key = singular, value = (plural, count)
    let mut counts: std::collections::BTreeMap<&str, (&str, usize)> =
        std::collections::BTreeMap::new();

    for event in events {
        let (singular, plural) = if event.kind.eq(&Kind::GitRepoAnnouncement) {
            ("announcement", "announcements")
        } else if event.kind.eq(&STATE_KIND) {
            ("state event", "state events")
        } else if event_is_cover_letter(event) {
            ("cover letter", "cover letters")
        } else if event.kind.eq(&Kind::GitPatch) {
            ("patch", "patches")
        } else if event.kind.eq(&KIND_PULL_REQUEST) {
            ("PR", "PRs")
        } else if event.kind.eq(&KIND_PULL_REQUEST_UPDATE) {
            ("PR update", "PR updates")
        } else if [
            Kind::GitStatusOpen,
            Kind::GitStatusDraft,
            Kind::GitStatusClosed,
            Kind::GitStatusApplied,
        ]
        .contains(&event.kind)
        {
            ("status update", "status updates")
        } else if event.kind.eq(&KIND_USER_GRASP_LIST) {
            ("user relay list", "user relay lists")
        } else {
            ("event", "events")
        };
        counts
            .entry(singular)
            .and_modify(|(_, c)| *c += 1)
            .or_insert((plural, 1));
    }

    let parts: Vec<String> = counts
        .iter()
        .map(|(singular, (plural, n))| {
            if *n == 1 {
                format!("1 {singular}")
            } else {
                format!("{n} {plural}")
            }
        })
        .collect();

    match parts.len() {
        0 => "0 events".to_string(),
        1 => parts[0].clone(),
        _ => {
            let (last, rest) = parts.split_last().unwrap();
            format!("{} and {last}", rest.join(", "))
        }
    }
}

pub async fn delete_event_from_local_cache(
    git_repo_path: &Path,
    event_id: nostr::prelude::EventId,
) -> Result<()> {
    let db = get_local_cache_database(git_repo_path).await?;
    db.delete(nostr::prelude::Filter::default().id(event_id))
        .await
        .map_err(|e| anyhow!("failed to delete event from local cache: {e}"))?;
    Ok(())
}

fn remove_trailing_slash(s: &str) -> String {
    match s.strip_suffix('/') {
        Some(s) => s,
        None => s,
    }
    .to_string()
}

fn events_repository_privacy(events: &[Event]) -> Option<bool> {
    let announcements = events
        .iter()
        .filter(|event| event.kind == Kind::GitRepoAnnouncement)
        .collect::<Vec<_>>();
    (!announcements.is_empty()).then(|| {
        announcements.iter().any(|event| {
            event
                .tags
                .iter()
                .any(|tag| tag.as_slice() == ["private".to_string(), "true".to_string()])
        })
    })
}

async fn private_for_publication(
    git_repo_path: Option<&Path>,
    events: &[Event],
    locally_private: bool,
) -> bool {
    let Some(identifier) = events
        .iter()
        .find(|event| event.kind == Kind::GitRepoAnnouncement)
        .and_then(|event| event.tags.identifier())
    else {
        return locally_private;
    };
    let Some(path) = git_repo_path else {
        return events_repository_privacy(events).unwrap_or(locally_private);
    };

    match publication_privacy_after_replacements(path, &identifier, events).await {
        Ok(Some(private)) => private,
        // Broadening publication is safe only after the complete recursive
        // maintainer set has been evaluated. A cache/configuration failure must
        // therefore preserve private routing.
        Ok(None) | Err(_) => locally_private || events_repository_privacy(events) == Some(true),
    }
}

async fn publication_privacy_after_replacements(
    git_repo_path: &Path,
    identifier: &str,
    replacements: &[Event],
) -> Result<Option<bool>> {
    use nostr::nips::nip19::FromBech32;

    let repository = git2::Repository::open(git_repo_path)?;
    let selected = Nip19Coordinate::from_bech32(
        &repository
            .config()?
            .get_string("nostr.repo")
            .context("nostr.repo is not configured")?,
    )?;
    if selected.identifier != identifier {
        return Ok(None);
    }

    let filter = nostr::prelude::Filter::default()
        .kind(Kind::GitRepoAnnouncement)
        .identifiers([identifier.to_string()]);
    let mut announcements =
        get_event_from_global_cache(Some(git_repo_path), vec![filter.clone()]).await?;
    announcements.extend(get_events_from_local_cache(git_repo_path, vec![filter]).await?);
    announcements.extend(replacements.iter().cloned());

    Ok(Some(repository_privacy_from_effective_announcements(
        selected.public_key,
        identifier,
        &announcements,
    )))
}

fn repository_privacy_from_effective_announcements(
    selected_maintainer: PublicKey,
    identifier: &str,
    announcements: &[Event],
) -> bool {
    let mut effective = HashMap::<PublicKey, &Event>::new();
    for candidate in announcements.iter().filter(|event| {
        event.kind == Kind::GitRepoAnnouncement
            && event.tags.identifier().is_some_and(|id| id == identifier)
    }) {
        let replace = effective.get(&candidate.pubkey).is_none_or(|current| {
            crate::event_ordering::latest_event([*current, candidate])
                .is_some_and(|latest| latest.id == candidate.id)
        });
        if replace {
            effective.insert(candidate.pubkey, candidate);
        }
    }

    let Some(selected_event) = effective.get(&selected_maintainer) else {
        return true;
    };
    let Ok(mut repo_ref) =
        RepoRef::try_from(((*selected_event).clone(), Some(selected_maintainer)))
    else {
        return true;
    };

    // Discover the same maintainer candidate closure as normal repository
    // loading. Candidate announcements are retained for reciprocity, but only
    // the confirmed subset contributes to the privacy decision below.
    let mut maintainers = vec![selected_maintainer];
    let mut seen = HashSet::from([selected_maintainer]);
    let mut cursor = 0;
    while cursor < maintainers.len() {
        let author = maintainers[cursor];
        cursor += 1;
        let Some(event) = effective.get(&author) else {
            continue;
        };
        let Ok(author_ref) = RepoRef::try_from(((*event).clone(), None)) else {
            return true;
        };
        for subject in author_ref.maintainers {
            if seen.insert(subject) {
                maintainers.push(subject);
            }
        }
    }

    repo_ref.maintainers = maintainers;
    repo_ref.events.clear();
    for author in &repo_ref.maintainers {
        if let Some(event) = effective.get(author) {
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: Kind::GitRepoAnnouncement,
                        public_key: *author,
                        identifier: identifier.to_string(),
                    },
                    relays: vec![],
                },
                (*event).clone(),
            );
        }
    }
    if !repo_ref
        .confirmed_maintainers()
        .contains(&selected_maintainer)
    {
        // The selected-non-member topology is deliberately unsupported for
        // this release. Privacy publication must fail closed until it can be
        // resolved safely.
        return true;
    }

    repo_ref.moderators = repo_ref.assigned_moderators();
    for moderator in repo_ref.moderators.clone() {
        if let Some(event) = effective.get(&moderator) {
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: Kind::GitRepoAnnouncement,
                        public_key: moderator,
                        identifier: identifier.to_string(),
                    },
                    relays: vec![],
                },
                (*event).clone(),
            );
        }
    }
    repo_ref.moderators.retain(|moderator| {
        effective
            .get(moderator)
            .is_none_or(|event| !announcement_author_declines_moderatorship(event))
    });

    repo_ref
        .confirmed_member_announcements()
        .iter()
        .any(|event| {
            RepoRef::try_from(((*event).clone(), None)).is_ok_and(|member_ref| member_ref.private)
        })
}

#[cfg(test)]
mod tests {
    use std::{io, sync::atomic::AtomicUsize};

    use indicatif::{ProgressDrawTarget, TermLike};
    use nostr::prelude::{
        Keys,
        event::{FinalizeUnsignedEvent, SignEvent},
    };

    use super::*;

    #[derive(Debug)]
    struct ClearTrackingTerm {
        clears: Arc<AtomicUsize>,
    }

    impl TermLike for ClearTrackingTerm {
        fn width(&self) -> u16 {
            80
        }

        fn move_cursor_up(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, _s: &str) -> io::Result<()> {
            Ok(())
        }

        fn write_str(&self, _s: &str) -> io::Result<()> {
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            self.clears.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_relay_failure_is_cleared_in_concise_mode() {
        let clears = Arc::new(AtomicUsize::new(0));
        let progress = MultiProgress::with_draw_target(ProgressDrawTarget::term_like(Box::new(
            ClearTrackingTerm {
                clears: clears.clone(),
            },
        )));
        let bar = progress.add(
            ProgressBar::new(1)
                .with_style(ProgressStyle::with_template("{msg}").expect("valid style")),
        );
        bar.finish_with_message("timeout after 7s timeout");
        let clears_before_finish = clears.load(Ordering::Relaxed);

        let reporter = RelayProgressReporter {
            details: Some(progress),
            mode: RelayProgressMode::Concise,
            _spinner_multi: None,
            spinner: None,
            heading: None,
            heading_message: RELAY_FETCH_HEADING.to_owned(),
            reveal_state: Some(Arc::new(BarRevealState {
                revealed: AtomicBool::new(false),
                deferred: Mutex::new(Vec::new()),
            })),
            timer_handle: None,
            fetch_relay_health: FetchRelayHealth {
                repository_relay_attempts: 1,
                repository_relay_successes: 1,
            },
            finished: false,
        };

        let had_errors = finish_fetch_progress::<FetchReport>(
            &[Err(anyhow!("relay timed out")), Ok(FetchReport::default())],
            reporter,
        )
        .expect("partial progress cleanup");

        assert!(had_errors);
        assert!(
            clears.load(Ordering::Relaxed) > clears_before_finish,
            "concise relay reads must clear transient detail after a partial outage"
        );
    }

    #[test]
    fn relay_progress_completion_policy_is_shared_by_fetch_and_publish() {
        assert!(!retain_relay_progress_details(
            RelayProgressMode::Concise,
            false
        ));
        assert!(retain_relay_progress_details(
            RelayProgressMode::Concise,
            true
        ));
        assert!(retain_relay_progress_details(
            RelayProgressMode::Detailed,
            false
        ));
        assert!(!retain_relay_progress_details(
            RelayProgressMode::Hidden,
            true
        ));
    }

    #[test]
    fn fetch_diagnostics_require_total_or_repository_outage() {
        assert!(!fetch_failure_requires_diagnostics(0, 0, 0, 0));
        assert!(!fetch_failure_requires_diagnostics(3, 2, 2, 1));
        assert!(fetch_failure_requires_diagnostics(3, 0, 0, 0));
        assert!(fetch_failure_requires_diagnostics(3, 1, 2, 0));
    }

    #[test]
    fn announcement_only_filters_request_only_repo_announcements_from_maintainers() {
        let public_key =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let coordinates = HashSet::from_iter([Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key,
                identifier: "repo".to_string(),
            },
            relays: vec![],
        }]);

        let filters = get_announcement_only_fetch_filters(&coordinates);

        assert_eq!(filters.len(), 1);
        assert_eq!(
            filters[0].kinds,
            Some(std::collections::BTreeSet::from_iter([
                Kind::GitRepoAnnouncement,
            ]))
        );
        assert_eq!(
            filters[0].authors,
            Some(std::collections::BTreeSet::from_iter([public_key]))
        );
    }

    #[test]
    fn announcement_only_filters_are_empty_without_coordinates() {
        assert!(get_announcement_only_fetch_filters(&HashSet::new()).is_empty());
    }

    #[test]
    fn auxiliary_filters_include_only_announcements_and_user_data() {
        let maintainer =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let profile =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();
        let coordinates = HashSet::from_iter([Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: maintainer,
                identifier: "repo".to_string(),
            },
            relays: vec![],
        }]);

        let filters =
            get_auxiliary_fetch_filters(&coordinates, &HashSet::from_iter([profile]), true, true);

        assert_eq!(filters.len(), 2);
        assert_eq!(
            filters[0].kinds,
            Some(std::collections::BTreeSet::from_iter([
                Kind::GitRepoAnnouncement,
            ]))
        );
        assert_eq!(
            filters[1].kinds,
            Some(std::collections::BTreeSet::from_iter([
                Kind::Metadata,
                Kind::RelayList,
                KIND_USER_GRASP_LIST,
            ]))
        );
        assert_eq!(
            filters[1].authors,
            Some(std::collections::BTreeSet::from_iter([profile]))
        );
    }

    #[tokio::test]
    async fn hintless_coordinate_schedules_its_author_relay_list() {
        let author = Keys::generate().public_key();
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: author,
                identifier: "repo".to_owned(),
            },
            relays: vec![],
        };

        let request = create_relays_request(
            None,
            Some(&coordinate),
            &HashSet::new(),
            &HashSet::new(),
            HashSet::new(),
            HashSet::new(),
            false,
        )
        .await
        .unwrap();

        assert!(
            request
                .profiles_to_fetch_from_user_relays
                .contains_key(&author)
        );
        assert_eq!(
            request.announcement_profile_authors,
            HashSet::from([author])
        );
    }

    #[tokio::test]
    async fn hinted_coordinate_does_not_bootstrap_its_author_profile() {
        let author = Keys::generate().public_key();
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: author,
                identifier: "repo".to_owned(),
            },
            relays: vec![RelayUrl::parse("wss://hint.example").unwrap()],
        };

        let request = create_relays_request(
            None,
            Some(&coordinate),
            &HashSet::new(),
            &HashSet::new(),
            HashSet::new(),
            HashSet::new(),
            false,
        )
        .await
        .unwrap();

        assert!(
            !request
                .profiles_to_fetch_from_user_relays
                .contains_key(&author)
        );
        assert!(request.announcement_profile_authors.is_empty());
    }

    #[test]
    fn announcement_indexer_bootstraps_only_the_hintless_author_profile() {
        let author = Keys::generate().public_key();
        let unrelated = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://indexer.example").unwrap();
        let request = FetchRequest {
            announcement_indexer_relays: HashSet::from([relay.clone()]),
            announcement_profile_authors: HashSet::from([author]),
            profiles_to_fetch_from_user_relays: HashMap::from([
                (
                    author,
                    (Timestamp::from(0), Timestamp::from(0), Timestamp::from(0)),
                ),
                (
                    unrelated,
                    (Timestamp::from(0), Timestamp::from(0), Timestamp::from(0)),
                ),
            ]),
            missing_contributor_profiles: HashSet::from([author, unrelated]),
            ..FetchRequest::default()
        };

        let scoped = request.scoped_to_relay(&relay);

        assert_eq!(
            scoped.scope,
            RelayFetchScope::Auxiliary {
                announcements: true,
                profiles: true,
            }
        );
        assert_eq!(scoped.missing_contributor_profiles, HashSet::from([author]));
        assert_eq!(
            scoped
                .profiles_to_fetch_from_user_relays
                .into_keys()
                .collect::<HashSet<_>>(),
            HashSet::from([author])
        );
    }

    #[test]
    fn author_write_relay_can_fetch_both_profile_and_announcement() {
        let author = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://author.example").unwrap();
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: author,
                identifier: "repo".to_owned(),
            },
            relays: vec![],
        };
        let request = FetchRequest {
            announcement_indexer_relays: HashSet::from([relay.clone()]),
            user_relays_for_profiles: HashSet::from([relay.clone()]),
            repo_coordinates_without_relays: vec![(coordinate, None)],
            profiles_to_fetch_from_user_relays: HashMap::from([(
                author,
                (Timestamp::from(0), Timestamp::from(0), Timestamp::from(0)),
            )]),
            ..FetchRequest::default()
        };

        let scoped = request.scoped_to_relay(&relay);

        assert_eq!(
            scoped.scope,
            RelayFetchScope::Auxiliary {
                announcements: true,
                profiles: true,
            }
        );
        assert_eq!(scoped.repo_coordinates_without_relays.len(), 1);
        assert!(
            scoped
                .profiles_to_fetch_from_user_relays
                .contains_key(&author)
        );
    }

    #[test]
    fn resolved_announcement_stops_mandatory_author_relay_discovery() {
        let author = Keys::generate().public_key();
        let indexer = RelayUrl::parse("wss://indexer.example").unwrap();
        let author_relay = RelayUrl::parse("wss://author.example").unwrap();
        let mut request = FetchRequest {
            announcement_indexer_relays: HashSet::from([indexer.clone(), author_relay.clone()]),
            author_announcement_relays: HashSet::from([author_relay]),
            announcement_profile_authors: HashSet::from([author]),
            ..FetchRequest::default()
        };

        request.complete_hintless_announcement_discovery();

        assert!(request.author_announcement_relays.is_empty());
        assert!(request.announcement_profile_authors.is_empty());
        assert!(
            request.announcement_indexer_relays.contains(&indexer),
            "completing bootstrap must not remove configured indexers"
        );
    }

    #[test]
    fn completed_bootstrap_does_not_reschedule_the_announcement_indexer() {
        let author = Keys::generate().public_key();
        let indexer = RelayUrl::parse("wss://indexer.example").unwrap();
        let mut request = FetchRequest {
            announcement_indexer_relays: HashSet::from([indexer.clone()]),
            announcement_profile_authors: HashSet::from([author]),
            ..FetchRequest::default()
        };

        let bootstrap_request = request.scoped_to_relay(&indexer);
        assert_eq!(
            bootstrap_request.scope,
            RelayFetchScope::Auxiliary {
                announcements: true,
                profiles: true,
            }
        );

        request.complete_hintless_announcement_discovery();
        let completed_request = request.scoped_to_relay(&indexer);
        assert_eq!(
            completed_request.scope,
            RelayFetchScope::Auxiliary {
                announcements: true,
                profiles: false,
            }
        );
        assert_eq!(
            bootstrap_request.relay_processing_key(),
            completed_request.relay_processing_key(),
            "bootstrap-only profile work must not change the processed relay key"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_deadlines_preserve_repository_work_after_auxiliary_successes() {
        let auxiliary = RelayFetchScope::Auxiliary {
            announcements: true,
            profiles: false,
        };
        let requests: Vec<_> = (0..10)
            .map(|index| FetchRequest {
                scope: if index < 5 {
                    RelayFetchScope::Repository
                } else {
                    auxiliary
                },
                ..FetchRequest::default()
            })
            .collect();
        let progress = FetchRoundProgress::new(&requests);
        for _ in 0..5 {
            progress.record_success(auxiliary);
        }
        let mut deadline = FetchDeadline::new(Duration::from_secs(45), Duration::from_secs(7));
        tokio::time::advance(Duration::from_secs(8)).await;
        deadline.update(
            tokio::time::Instant::now(),
            progress.has_quorum(RelayFetchScope::Repository),
        );
        assert_eq!(deadline.budget(), Duration::from_secs(45));
        // Three of five repository peers establish actual repository redundancy.
        for _ in 0..3 {
            progress.record_success(RelayFetchScope::Repository);
        }
        deadline.update(
            tokio::time::Instant::now(),
            progress.has_quorum(RelayFetchScope::Repository),
        );
        assert_eq!(deadline.budget(), Duration::from_secs(15));
        // Subsequent discovery work must not inherit the previous round's quorum.
        let next_round = FetchRoundProgress::new(&requests[..1]);
        assert!(!next_round.has_quorum(RelayFetchScope::Repository));
        assert!(!next_round.has_quorum(auxiliary));
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_deadlines_shorten_isolated_metadata_after_overall_quorum() {
        let auxiliary = RelayFetchScope::Auxiliary {
            announcements: false,
            profiles: true,
        };
        let requests: Vec<_> = [
            RelayFetchScope::Repository,
            RelayFetchScope::Repository,
            auxiliary,
        ]
        .into_iter()
        .map(|scope| FetchRequest {
            scope,
            ..FetchRequest::default()
        })
        .collect();
        let progress = FetchRoundProgress::new(&requests);
        let mut deadline = FetchDeadline::new(Duration::from_secs(45), Duration::from_secs(7));
        progress.record_success(RelayFetchScope::Repository);
        assert!(!progress.has_quorum(auxiliary));
        progress.record_success(RelayFetchScope::Repository);
        tokio::time::advance(Duration::from_secs(2)).await;
        deadline.update(tokio::time::Instant::now(), progress.has_quorum(auxiliary));
        assert_eq!(deadline.budget(), Duration::from_secs(9));

        // Metadata-only discovery still uses the overall threshold, even
        // when the requests have different auxiliary scopes.
        let discovery = RelayFetchScope::Auxiliary {
            announcements: true,
            profiles: false,
        };
        let requests = [
            FetchRequest {
                scope: auxiliary,
                ..FetchRequest::default()
            },
            FetchRequest {
                scope: discovery,
                ..FetchRequest::default()
            },
        ];
        let progress = FetchRoundProgress::new(&requests);
        progress.record_success(discovery);
        assert!(progress.has_quorum(auxiliary));
        assert!(!progress.has_quorum(RelayFetchScope::Repository));
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_deadline_is_absolute_and_never_slides_or_extends() {
        let mut deadline = FetchDeadline::new(Duration::from_secs(45), Duration::from_secs(7));
        tokio::time::advance(Duration::from_secs(44)).await;
        deadline.update(tokio::time::Instant::now(), true);
        assert_eq!(deadline.budget(), Duration::from_secs(45));
        let mut early = FetchDeadline::new(Duration::from_secs(45), Duration::from_secs(7));
        early.update(tokio::time::Instant::now(), true);
        tokio::time::advance(Duration::from_secs(3)).await;
        early.update(tokio::time::Instant::now(), true);
        assert_eq!(early.budget(), Duration::from_secs(7));
    }

    #[test]
    fn sibling_resolution_releases_author_relay_to_the_adaptive_timeout() {
        let announcement_resolved = AtomicBool::new(false);

        assert!(author_announcement_discovery_pending(
            true,
            &announcement_resolved
        ));
        announcement_resolved.store(true, Ordering::Release);
        assert!(!author_announcement_discovery_pending(
            true,
            &announcement_resolved
        ));
        assert!(!author_announcement_discovery_pending(
            false,
            &AtomicBool::new(false)
        ));
    }

    #[test]
    fn mailbox_request_combines_profile_and_private_relay_list_filters() {
        let public_key =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let mailbox = RelayUrl::parse("wss://mailbox.example").unwrap();
        let request = FetchRequest {
            selected_relay: Some(mailbox.clone()),
            user_relays_for_profiles: HashSet::from_iter([mailbox]),
            private_relay_list_authors: HashSet::from_iter([public_key]),
            ..FetchRequest::default()
        };
        let mut filters = get_auxiliary_fetch_filters(
            &HashSet::new(),
            &HashSet::from_iter([public_key]),
            false,
            true,
        );

        request.add_private_relay_list_filter(&mut filters);

        assert_eq!(filters.len(), 2);
        assert_eq!(
            filters[0].kinds,
            Some(std::collections::BTreeSet::from_iter([
                Kind::Metadata,
                Kind::RelayList,
                KIND_USER_GRASP_LIST,
            ]))
        );
        assert_eq!(
            filters[1].kinds,
            Some(std::collections::BTreeSet::from_iter([
                KIND_PRIVATE_GIT_RELAY_LIST,
            ]))
        );
        assert_eq!(
            filters[1].authors,
            Some(std::collections::BTreeSet::from_iter([public_key]))
        );
    }

    #[test]
    fn private_relay_list_filter_is_not_sent_to_non_mailbox_relays() {
        let public_key =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let request = FetchRequest {
            selected_relay: Some(RelayUrl::parse("wss://indexer.example").unwrap()),
            user_relays_for_profiles: HashSet::from_iter([RelayUrl::parse(
                "wss://mailbox.example",
            )
            .unwrap()]),
            private_relay_list_authors: HashSet::from_iter([public_key]),
            ..FetchRequest::default()
        };
        let mut filters = Vec::new();

        request.add_private_relay_list_filter(&mut filters);

        assert!(filters.is_empty());
    }

    #[tokio::test]
    async fn global_memory_fallback_retains_events_for_the_process() {
        let database = use_in_memory_global_cache(anyhow!("test cache failure"));
        let keys = nostr::prelude::Keys::generate();
        let event = keys
            .sign_event(
                EventBuilder::new(Kind::TextNote, "cached in memory")
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();

        database.save_event(&event).await.unwrap();

        assert_eq!(database.event_by_id(&event.id).await.unwrap(), Some(event));
    }

    mod event_rejection_is_duplicate {
        use super::*;

        /// An `OK: false` relay response surfaces from the SDK as a
        /// `Rejected`-kind error whose display is the raw relay message.
        fn rejection(message: &str) -> NostrSdkError {
            NostrSdkError::new(NostrSdkErrorKind::Rejected, message.to_string())
        }

        #[test]
        fn duplicate_rejection_is_detected() {
            assert!(event_rejection_is_duplicate(&rejection(
                "duplicate: already have this event"
            )));
            assert!(event_rejection_is_duplicate(&rejection("duplicate:")));
        }

        #[test]
        fn other_rejections_are_not_duplicates() {
            assert!(!event_rejection_is_duplicate(&rejection(
                "invalid: event signature check failed"
            )));
            assert!(!event_rejection_is_duplicate(&rejection(
                "blocked: pubkey not welcome"
            )));
            assert!(!event_rejection_is_duplicate(&rejection(
                "rejected without machine-readable prefix"
            )));
        }

        #[test]
        fn duplicate_message_on_a_non_rejection_error_is_not_a_duplicate() {
            // e.g. a transport failure must never be promoted to
            // delivery success, whatever its message says
            assert!(!event_rejection_is_duplicate(&NostrSdkError::new(
                NostrSdkErrorKind::Transport,
                "duplicate: already have this event".to_string(),
            )));
        }
    }
}

#[cfg(test)]
mod tor_proxy_tests {
    use std::net::{SocketAddr, TcpListener};

    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    #[test]
    fn discovers_system_tor_before_tor_browser() {
        let found = discover_tor_socks5_proxy(None, |candidate| {
            candidate == addr(DEFAULT_TOR_SOCKS5_PROXY)
        });
        assert_eq!(found, Some(addr(DEFAULT_TOR_SOCKS5_PROXY)));
    }

    #[test]
    fn falls_back_to_tor_browser_proxy() {
        let found = discover_tor_socks5_proxy(None, |candidate| {
            candidate == addr(TOR_BROWSER_SOCKS5_PROXY)
        });
        assert_eq!(found, Some(addr(TOR_BROWSER_SOCKS5_PROXY)));
    }

    #[test]
    fn explicit_proxy_does_not_fall_back_to_defaults() {
        let configured = "127.0.0.1:19050";
        let mut probed = Vec::new();
        let found = discover_tor_socks5_proxy(Some(configured), |candidate| {
            probed.push(candidate);
            false
        });
        assert_eq!(found, None);
        assert_eq!(probed, vec![addr(configured)]);
    }

    #[test]
    fn disabled_proxy_does_not_probe() {
        let found =
            discover_tor_socks5_proxy(Some("off"), |_| panic!("disabled proxy must not be probed"));
        assert_eq!(found, None);
    }

    #[test]
    fn detects_a_listening_proxy_without_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let found = discover_tor_socks5_proxy(Some(&listener_addr.to_string()), |candidate| {
            std::net::TcpStream::connect_timeout(&candidate, TOR_PROXY_PROBE_TIMEOUT).is_ok()
        });
        assert_eq!(found, Some(listener_addr));
    }
}

#[cfg(test)]
mod private_repository_tests {
    use nostr::prelude::{
        EventBuilder, Keys, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
    };

    use super::*;

    fn signed(keys: &Keys, builder: EventBuilder) -> Event {
        keys.sign_event(builder.finalize_unsigned(keys.public_key()))
            .unwrap()
    }

    #[test]
    fn account_scoped_publication_ignores_repository_only_routing() {
        assert!(!repository_only_routing(false, true, true));
        assert!(repository_only_routing(true, true, false));
        assert!(repository_only_routing(true, false, true));
        assert!(!repository_only_routing(true, false, false));
    }

    #[tokio::test]
    async fn private_publication_requires_an_exact_private_tag() {
        fn announcement(tag: Option<&[&str]>) -> Event {
            let keys = Keys::generate();
            let mut tags = vec![Tag::identifier("repo")];
            if let Some(tag) = tag {
                tags.push(Tag::parse(tag.iter().copied()).unwrap());
            }
            signed(
                &keys,
                EventBuilder::new(Kind::GitRepoAnnouncement, "").tags(tags),
            )
        }

        assert_eq!(
            events_repository_privacy(&[announcement(Some(&["private", "true"]))]),
            Some(true)
        );
        for tag in [
            None,
            Some(&["private", "false"][..]),
            Some(&["private", "TRUE"][..]),
            Some(&["private", "true", "extra"][..]),
        ] {
            assert_eq!(events_repository_privacy(&[announcement(tag)]), Some(false));
        }
        let keys = Keys::generate();
        let related = signed(&keys, EventBuilder::new(Kind::TextNote, "related"));
        assert_eq!(
            events_repository_privacy(std::slice::from_ref(&related)),
            None
        );
        assert!(
            !private_for_publication(None, &[announcement(None)], true).await,
            "a public replacement must override stale local private state"
        );
        assert!(
            private_for_publication(None, &[related], true).await,
            "related events must inherit current local private state"
        );
    }

    #[test]
    fn recursive_maintainer_events_are_private_when_any_announcement_is_private() {
        fn announcement(keys: &Keys, private: bool) -> Event {
            let mut tags = vec![Tag::identifier("repo")];
            if private {
                tags.push(Tag::parse(["private", "true"]).unwrap());
            }
            keys.sign_event(
                EventBuilder::new(Kind::GitRepoAnnouncement, "")
                    .tags(tags)
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap()
        }

        let maintainers = [Keys::generate(), Keys::generate(), Keys::generate()];
        let public_events = maintainers
            .iter()
            .map(|keys| announcement(keys, false))
            .collect::<Vec<_>>();
        assert!(!repository_events_are_private(&public_events));

        let mut recursive_events = public_events;
        recursive_events[2] = announcement(&maintainers[2], true);
        assert!(repository_events_are_private(&recursive_events));
    }

    #[test]
    fn cached_private_repository_always_restricts_relay_discovery() {
        assert!(restrict_repository_relays(false, true));
        assert!(restrict_repository_relays(true, false));
        assert!(!restrict_repository_relays(false, false));
        assert!(!coordinate_hints_are_allowed(true));
        assert!(coordinate_hints_are_allowed(false));
    }

    #[test]
    fn repository_privacy_is_persisted_and_updated_in_git_config() {
        let dir = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init(dir.path()).unwrap();
        save_repository_privacy_to_git_config(dir.path(), true);
        assert!(
            repository
                .config()
                .unwrap()
                .get_bool("nostr.private")
                .unwrap()
        );
        save_repository_privacy_to_git_config(dir.path(), false);
        assert!(
            !repository
                .config()
                .unwrap()
                .get_bool("nostr.private")
                .unwrap()
        );
    }

    #[test]
    fn repository_privacy_persistence_degrades_to_warning_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        save_repository_privacy_to_git_config(&dir.path().join("missing"), true);
    }

    /// An unchanged value must not open `.git/config` for writing: read-only
    /// flows and concurrent remote-helper processes would otherwise contend
    /// on `config.lock`, and a failed write must degrade to a warning.
    #[test]
    fn unchanged_repository_privacy_is_not_rewritten_and_write_failures_degrade() {
        let dir = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init(dir.path()).unwrap();
        save_repository_privacy_to_git_config(dir.path(), true);

        let git_dir = repository.path().to_path_buf();
        let config_lock = git_dir.join("config.lock");
        std::fs::File::create(&config_lock).unwrap();

        // same value: no write is attempted, so the existing lock is fine
        save_repository_privacy_to_git_config(dir.path(), true);
        // changed value: the failed write warns instead of failing the flow
        save_repository_privacy_to_git_config(dir.path(), false);

        std::fs::remove_file(config_lock).unwrap();
        assert!(
            repository
                .config()
                .unwrap()
                .get_bool("nostr.private")
                .unwrap()
        );
    }

    #[test]
    fn successful_empty_private_probe_allows_public_fallback() {
        let outcome =
            consolidate_fetch_outcome(vec![Ok(FetchReport::default()), Ok(FetchReport::default())]);
        assert!(!outcome.had_errors);
        assert_eq!(
            private_relay_probe_decision(None, outcome.all_required_relays_completed(2)),
            PrivateRelayProbeDecision::RetryPublicDiscovery
        );
    }

    #[test]
    fn partial_and_complete_private_probe_failures_never_allow_public_fallback() {
        for outcome in [
            consolidate_fetch_outcome(vec![
                Ok(FetchReport::default()),
                Err(anyhow!("relay b unavailable")),
            ]),
            consolidate_fetch_outcome(vec![
                Err(anyhow!("relay a unavailable")),
                Err(anyhow!("relay b unavailable")),
            ]),
        ] {
            assert!(outcome.had_errors);
            assert_eq!(
                private_relay_probe_decision(None, outcome.all_required_relays_completed(2)),
                PrivateRelayProbeDecision::FailClosed
            );
        }
        let skipped = consolidate_fetch_outcome(vec![]);
        assert_eq!(
            private_relay_probe_decision(None, skipped.all_required_relays_completed(1)),
            PrivateRelayProbeDecision::FailClosed
        );
    }

    #[test]
    fn publication_privacy_uses_post_replacement_recursive_maintainers() {
        fn announcement(
            keys: &Keys,
            maintainers: &[PublicKey],
            private: bool,
            created_at: u64,
        ) -> Event {
            let mut tags = vec![
                Tag::identifier("repo"),
                Tag::parse(
                    [
                        vec!["maintainers".to_string()],
                        maintainers.iter().map(ToString::to_string).collect(),
                    ]
                    .concat(),
                )
                .unwrap(),
            ];
            if private {
                tags.push(Tag::parse(["private", "true"]).unwrap());
            }
            signed(
                keys,
                EventBuilder::new(Kind::GitRepoAnnouncement, "")
                    .tags(tags)
                    .custom_created_at(Timestamp::from_secs(created_at)),
            )
        }

        let alice = Keys::generate();
        let bob = Keys::generate();
        let current = vec![
            announcement(&alice, &[alice.public_key(), bob.public_key()], true, 1),
            announcement(&bob, &[alice.public_key(), bob.public_key()], true, 1),
        ];
        let alice_public_replacement =
            announcement(&alice, &[alice.public_key(), bob.public_key()], false, 2);
        assert!(
            repository_privacy_from_effective_announcements(
                alice.public_key(),
                "repo",
                &[current.clone(), vec![alice_public_replacement.clone()]].concat(),
            ),
            "one maintainer's public replacement must not leak while another current announcement remains private"
        );

        let bob_public_replacement =
            announcement(&bob, &[alice.public_key(), bob.public_key()], false, 2);
        assert!(
            !repository_privacy_from_effective_announcements(
                alice.public_key(),
                "repo",
                &[
                    current,
                    vec![alice_public_replacement, bob_public_replacement],
                ]
                .concat(),
            ),
            "publication may broaden only after every reachable current announcement is public"
        );
    }

    #[test]
    fn invited_maintainer_cannot_change_publication_privacy() {
        fn announcement(
            keys: &Keys,
            maintainers: &[PublicKey],
            private: bool,
            created_at: u64,
        ) -> Event {
            let mut tags = vec![
                Tag::identifier("repo"),
                Tag::parse(
                    [
                        vec!["maintainers".to_string()],
                        maintainers.iter().map(ToString::to_string).collect(),
                    ]
                    .concat(),
                )
                .unwrap(),
            ];
            if private {
                tags.push(Tag::parse(["private", "true"]).unwrap());
            }
            signed(
                keys,
                EventBuilder::new(Kind::GitRepoAnnouncement, "")
                    .tags(tags)
                    .custom_created_at(Timestamp::from_secs(created_at)),
            )
        }

        let alice = Keys::generate();
        let bob = Keys::generate();
        let announcements = vec![
            announcement(&alice, &[alice.public_key(), bob.public_key()], false, 1),
            // Bob has a same-identifier private repository but has not
            // reciprocated Alice's invitation.
            announcement(&bob, &[bob.public_key()], true, 2),
        ];

        assert!(
            !repository_privacy_from_effective_announcements(
                alice.public_key(),
                "repo",
                &announcements,
            ),
            "an invitation must not import the invitee's privacy setting"
        );
    }
}

#[cfg(test)]
mod moderator_discovery_tests {
    use nostr::prelude::{EventBuilder, Keys, Tag, event::FinalizeEvent};

    use super::*;

    fn announcement(keys: &Keys, role_tags: &[Vec<String>]) -> Event {
        let mut tags = vec![Tag::identifier("repo")];
        for tag in role_tags {
            tags.push(Tag::parse(tag.clone()).unwrap());
        }
        EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .finalize(keys)
            .unwrap()
    }

    fn coordinate(public_key: PublicKey) -> Nip19Coordinate {
        Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key,
                identifier: "repo".to_string(),
            },
            relays: vec![],
        }
    }

    async fn consolidated_with_moderator_announcement(
        moderator_role_tags: impl FnOnce(&str, &str) -> Vec<Vec<String>>,
    ) -> (RepoRef, PublicKey, PublicKey) {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();

        let owner_keys = Keys::generate();
        let owner = owner_keys.public_key();
        let moderator_keys = Keys::generate();
        let moderator = moderator_keys.public_key();
        let owner_hex = owner.to_string();
        let moderator_hex = moderator.to_string();

        save_event_in_local_cache(
            dir.path(),
            &announcement(
                &owner_keys,
                &[
                    vec!["M".to_string(), owner_hex.clone()],
                    vec!["o".to_string(), moderator_hex.clone()],
                ],
            ),
        )
        .await
        .unwrap();
        save_event_in_local_cache(
            dir.path(),
            &announcement(
                &moderator_keys,
                &moderator_role_tags(&owner_hex, &moderator_hex),
            ),
        )
        .await
        .unwrap();

        let repo_ref = get_repo_ref_from_cache(Some(dir.path()), &coordinate(owner))
            .await
            .unwrap();
        (repo_ref, owner, moderator)
    }

    /// The moderator never appears in a maintainer listing, so their
    /// acknowledgement announcement is only found by following the `o`
    /// assignment in the owner's announcement.
    #[tokio::test]
    async fn moderator_announcements_are_discovered_from_o_assignments() {
        let (repo_ref, _, moderator) = consolidated_with_moderator_announcement(|owner, this| {
            vec![
                vec!["M".to_string(), owner.to_string()],
                vec!["o".to_string(), this.to_string()],
            ]
        })
        .await;

        assert_eq!(repo_ref.moderators, vec![moderator]);
        assert_eq!(repo_ref.confirmed_moderators(), vec![moderator]);
        assert!(repo_ref.is_authorized_member(&moderator));
        // moderators never join the maintainer set or gain state authority
        assert!(!repo_ref.maintainers.contains(&moderator));
        assert!(!repo_ref.is_authorized_maintainer(&moderator));
    }

    /// A leave (ended self-`o`) takes precedence over the still-active
    /// assignment even though the leaver's announcement sits outside the
    /// maintainer listings.
    #[tokio::test]
    async fn moderator_leave_is_discovered_without_a_maintainer_listing() {
        let (repo_ref, _, _) = consolidated_with_moderator_announcement(|owner, this| {
            vec![
                vec!["M".to_string(), owner.to_string()],
                vec![
                    "o".to_string(),
                    this.to_string(),
                    "0".to_string(),
                    "100".to_string(),
                ],
            ]
        })
        .await;

        assert!(repo_ref.moderators.is_empty());
        assert!(repo_ref.confirmed_moderators().is_empty());
    }

    /// Role assignments in the moderator's own announcement assign nothing:
    /// neither their `m` listing nor their `o` assignment confers anything
    /// on other pubkeys.
    #[tokio::test]
    async fn moderator_assignments_confer_nothing_on_consolidation() {
        let crony = Keys::generate().public_key();
        let (repo_ref, owner, moderator) =
            consolidated_with_moderator_announcement(move |owner, this| {
                vec![
                    vec!["M".to_string(), owner.to_string()],
                    vec!["o".to_string(), this.to_string()],
                    vec!["m".to_string(), crony.to_string()],
                    vec!["o".to_string(), crony.to_string()],
                ]
            })
            .await;

        assert_eq!(repo_ref.maintainers, vec![owner]);
        assert_eq!(repo_ref.moderators, vec![moderator]);
        assert!(!repo_ref.is_authorized_member(&crony));
    }
}

#[cfg(test)]
mod announcement_consolidation_tests {
    use nostr::prelude::{EventBuilder, Keys, Tag, event::FinalizeEvent};

    use super::*;

    fn announcement_at(keys: &Keys, created_at: u64, content: &str) -> Event {
        EventBuilder::new(Kind::GitRepoAnnouncement, content)
            .tags([Tag::identifier("repo")])
            .custom_created_at(Timestamp::from(created_at))
            .finalize(keys)
            .unwrap()
    }

    /// Only the author's latest announcement survives, whichever order the
    /// caches returned the versions in.
    #[test]
    fn latest_event_per_author_survives() {
        let keys = Keys::generate();
        let old = announcement_at(&keys, 100, "old");
        let new = announcement_at(&keys, 200, "new");

        for events in [
            vec![old.clone(), new.clone()],
            vec![new.clone(), old.clone()],
        ] {
            let consolidated = latest_announcement_per_author(events);
            assert_eq!(consolidated.len(), 1);
            assert_eq!(consolidated[0].id, new.id);
        }
    }

    /// A `created_at` tie keeps the lower event id, per NIP-01
    /// addressable-event rules.
    #[test]
    fn created_at_ties_keep_the_lowest_event_id() {
        let keys = Keys::generate();
        let a = announcement_at(&keys, 100, "a");
        let b = announcement_at(&keys, 100, "b");
        let winner_id = std::cmp::min(a.id, b.id);

        for events in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
            let consolidated = latest_announcement_per_author(events);
            assert_eq!(consolidated.len(), 1);
            assert_eq!(consolidated[0].id, winner_id);
        }
    }

    /// Ascending order with the NIP-01 winner of a cross-author tie last,
    /// so the latest-metadata `.last()` pick is deterministic.
    #[test]
    fn sorted_ascending_with_the_canonical_winner_of_a_tie_last() {
        let alice = announcement_at(&Keys::generate(), 100, "alice");
        let bob = announcement_at(&Keys::generate(), 50, "bob");
        let carol = announcement_at(&Keys::generate(), 100, "carol");

        let tie_winner_id = std::cmp::min(alice.id, carol.id);
        for events in [
            vec![alice.clone(), bob.clone(), carol.clone()],
            vec![carol.clone(), alice.clone(), bob.clone()],
        ] {
            let consolidated = latest_announcement_per_author(events);
            assert_eq!(consolidated.len(), 3);
            assert_eq!(consolidated[0].id, bob.id);
            assert_eq!(consolidated[2].id, tie_winner_id);
        }
    }
}

#[cfg(test)]
mod confirmed_repository_data_tests {
    use nostr::prelude::{EventBuilder, Keys, Tag, event::FinalizeEvent};

    use super::*;

    struct Announcement<'a> {
        created_at: u64,
        name: &'a str,
        clone_url: &'a str,
        relay: &'a str,
        blossom: &'a str,
        private: bool,
        roles: Vec<Vec<String>>,
    }

    fn announcement(keys: &Keys, values: Announcement<'_>) -> Event {
        let mut tags = vec![
            Tag::identifier("repo"),
            Tag::parse(["name", values.name]).unwrap(),
            Tag::parse(["clone", values.clone_url]).unwrap(),
            Tag::parse(["relays", values.relay]).unwrap(),
            Tag::parse(["blossoms", values.blossom]).unwrap(),
        ];
        if values.private {
            tags.push(Tag::parse(["private", "true"]).unwrap());
        }
        tags.extend(
            values
                .roles
                .into_iter()
                .map(|role| Tag::parse(role).unwrap()),
        );
        EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(values.created_at))
            .finalize(keys)
            .unwrap()
    }

    #[tokio::test]
    async fn invited_and_departed_announcements_are_discovery_only() {
        let owner_keys = Keys::generate();
        let invitee_keys = Keys::generate();
        let departed_keys = Keys::generate();
        let moderator_keys = Keys::generate();
        let owner = owner_keys.public_key();
        let invitee = invitee_keys.public_key();
        let departed = departed_keys.public_key();
        let moderator = moderator_keys.public_key();

        let events = vec![
            announcement(
                &owner_keys,
                Announcement {
                    created_at: 10,
                    name: "owner metadata",
                    clone_url: "https://owner.example/repo.git",
                    relay: "wss://owner.example",
                    blossom: "https://owner.example/blossom",
                    private: false,
                    roles: vec![
                        vec!["M".to_string(), owner.to_string()],
                        vec!["m".to_string(), invitee.to_string()],
                        vec!["m".to_string(), departed.to_string()],
                        vec!["o".to_string(), moderator.to_string()],
                    ],
                },
            ),
            announcement(
                &invitee_keys,
                Announcement {
                    created_at: 40,
                    name: "invitee metadata",
                    clone_url: "https://invitee.example/repo.git",
                    relay: "wss://invitee.example",
                    blossom: "https://invitee.example/blossom",
                    private: true,
                    // The invalid maintainer self-defer is not a departure,
                    // even alongside a valid moderator self-role. It remains
                    // repairable as the owner's invitation without granting
                    // current authority.
                    roles: vec![
                        vec!["M".to_string(), owner.to_string(), "20".to_string()],
                        vec![
                            "m".to_string(),
                            invitee.to_string(),
                            "20".to_string(),
                            "defer".to_string(),
                        ],
                        vec!["o".to_string(), invitee.to_string(), "10".to_string()],
                    ],
                },
            ),
            announcement(
                &departed_keys,
                Announcement {
                    created_at: 50,
                    name: "departed metadata",
                    clone_url: "https://departed.example/repo.git",
                    relay: "wss://departed.example",
                    blossom: "https://departed.example/blossom",
                    private: true,
                    roles: vec![
                        vec![
                            "m".to_string(),
                            departed.to_string(),
                            "1".to_string(),
                            "2".to_string(),
                        ],
                        vec![
                            "o".to_string(),
                            departed.to_string(),
                            "1".to_string(),
                            "defer".to_string(),
                        ],
                    ],
                },
            ),
            announcement(
                &moderator_keys,
                Announcement {
                    created_at: 60,
                    name: "moderator metadata",
                    clone_url: "https://moderator.example/repo.git",
                    relay: "wss://moderator.example",
                    blossom: "https://moderator.example/blossom",
                    private: true,
                    // The self acknowledgement does not name a confirmed
                    // member, so the moderator is not confirmed.
                    roles: vec![vec!["o".to_string(), moderator.to_string()]],
                },
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        for event in events {
            save_event_in_local_cache(dir.path(), &event).await.unwrap();
        }
        let repo_ref = get_repo_ref_from_cache(
            Some(dir.path()),
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: owner,
                    identifier: "repo".to_string(),
                },
                relays: vec![],
            },
        )
        .await
        .unwrap();

        assert_eq!(repo_ref.confirmed_maintainers(), vec![owner]);
        assert!(repo_ref.invited_maintainers().contains(&invitee));
        assert!(repo_ref.confirmed_moderators().is_empty());
        assert_eq!(repo_ref.name, "owner metadata");
        assert!(!repo_ref.private);
        assert_eq!(repo_ref.git_server, vec!["https://owner.example/repo.git"]);
        assert_eq!(
            repo_ref
                .relays
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["wss://owner.example"]
        );
        assert_eq!(
            repo_ref
                .blossoms
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["https://owner.example/blossom"]
        );
        assert!(
            repo_ref
                .events
                .values()
                .any(|event| event.pubkey == invitee),
            "the invitee announcement remains available for reciprocity"
        );
        assert!(
            repo_ref
                .events
                .values()
                .any(|event| event.pubkey == moderator),
            "the moderator announcement remains available for acknowledgement"
        );
        assert!(!repo_ref.maintainers.contains(&departed));
        assert!(!repo_ref.invited_maintainers().contains(&departed));
        assert!(
            repo_ref
                .events
                .values()
                .any(|event| event.pubkey == departed),
            "the invalid moderator self-defer remains available for health without hiding the valid maintainer departure"
        );
        assert_eq!(repo_ref.members_for_announcement_tags(), vec![owner]);
        assert_eq!(
            repo_ref
                .coordinates()
                .into_iter()
                .map(|coordinate| coordinate.public_key)
                .collect::<HashSet<_>>(),
            HashSet::from([owner])
        );
    }

    #[tokio::test]
    async fn departed_selected_coordinate_seeds_no_member_authority() {
        let selected_keys = Keys::generate();
        let lead_keys = Keys::generate();
        let selected = selected_keys.public_key();
        let lead = lead_keys.public_key();
        let events = [
            announcement(
                &selected_keys,
                Announcement {
                    created_at: 10,
                    name: "departed selected coordinate",
                    clone_url: "https://selected.example/repo.git",
                    relay: "wss://selected.example",
                    blossom: "https://selected.example/blossom",
                    private: false,
                    roles: vec![
                        vec!["M".to_string(), lead.to_string(), "5".to_string()],
                        vec![
                            "m".to_string(),
                            selected.to_string(),
                            "1".to_string(),
                            "9".to_string(),
                        ],
                    ],
                },
            ),
            announcement(
                &lead_keys,
                Announcement {
                    created_at: 11,
                    name: "new lead",
                    clone_url: "https://lead.example/repo.git",
                    relay: "wss://lead.example",
                    blossom: "https://lead.example/blossom",
                    private: false,
                    roles: vec![
                        vec!["M".to_string(), lead.to_string(), "5".to_string()],
                        vec![
                            "m".to_string(),
                            selected.to_string(),
                            "1".to_string(),
                            "9".to_string(),
                        ],
                    ],
                },
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        for event in events {
            save_event_in_local_cache(dir.path(), &event).await.unwrap();
        }

        let error = match get_repo_ref_from_cache(
            Some(dir.path()),
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: selected,
                    identifier: "repo".to_string(),
                },
                relays: vec![],
            },
        )
        .await
        {
            Ok(_) => panic!("a departed selected coordinate must fail closed"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(
                "selected repository coordinate author is no longer a confirmed maintainer"
            ),
        );
    }

    #[tokio::test]
    async fn invalid_self_defer_selected_coordinate_remains_readable_without_authority() {
        let selected_keys = Keys::generate();
        let selected = selected_keys.public_key();
        let event = announcement(
            &selected_keys,
            Announcement {
                created_at: 10,
                name: "unresolved selected coordinate",
                clone_url: "https://selected.example/repo.git",
                relay: "wss://selected.example",
                blossom: "https://selected.example/blossom",
                private: true,
                roles: vec![vec![
                    "M".to_string(),
                    selected.to_string(),
                    "5".to_string(),
                    "defer".to_string(),
                ]],
            },
        );
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        save_event_in_local_cache(dir.path(), &event).await.unwrap();

        let repo_ref = get_repo_ref_from_cache(
            Some(dir.path()),
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: selected,
                    identifier: "repo".to_string(),
                },
                relays: vec![],
            },
        )
        .await
        .unwrap();

        assert!(repo_ref.confirmed_maintainers().is_empty());
        assert!(!repo_ref.is_authorized_maintainer(&selected));
        assert!(repo_ref.invalid_self_defer_blocks(&selected));
        assert_eq!(repo_ref.name, "unresolved selected coordinate");
        assert!(repo_ref.private);
        assert_eq!(
            repo_ref.git_server,
            vec!["https://selected.example/repo.git"]
        );
        assert_eq!(
            repo_ref
                .relays
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["wss://selected.example"]
        );
    }

    #[tokio::test]
    async fn removed_selected_coordinate_with_stale_self_role_fails_closed() {
        let selected_keys = Keys::generate();
        let lead_keys = Keys::generate();
        let selected = selected_keys.public_key();
        let lead = lead_keys.public_key();
        let events = [
            announcement(
                &selected_keys,
                Announcement {
                    created_at: 10,
                    name: "stale selected co-maintainer",
                    clone_url: "https://selected.example/repo.git",
                    relay: "wss://selected.example",
                    blossom: "https://selected.example/blossom",
                    private: false,
                    roles: vec![
                        vec!["M".to_string(), lead.to_string(), "2".to_string()],
                        vec!["m".to_string(), selected.to_string(), "2".to_string()],
                    ],
                },
            ),
            announcement(
                &lead_keys,
                Announcement {
                    created_at: 11,
                    name: "lead after removal",
                    clone_url: "https://lead.example/repo.git",
                    relay: "wss://lead.example",
                    blossom: "https://lead.example/blossom",
                    private: false,
                    roles: vec![
                        vec!["M".to_string(), lead.to_string(), "1".to_string()],
                        vec![
                            "m".to_string(),
                            selected.to_string(),
                            "2".to_string(),
                            "9".to_string(),
                        ],
                    ],
                },
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        for event in events {
            save_event_in_local_cache(dir.path(), &event).await.unwrap();
        }

        let error = match get_repo_ref_from_cache(
            Some(dir.path()),
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: selected,
                    identifier: "repo".to_string(),
                },
                relays: vec![],
            },
        )
        .await
        {
            Ok(_) => panic!("a stale selected co-maintainer must fail closed after removal"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(
                "selected repository coordinate author is no longer a confirmed maintainer"
            ),
        );

        let recovery = get_repo_ref_from_cache_for_lead_recovery(
            Some(dir.path()),
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: selected,
                    identifier: "repo".to_string(),
                },
                relays: vec![],
            },
        )
        .await
        .unwrap();
        assert_eq!(recovery.confirmed_maintainers(), vec![lead]);
        assert!(!recovery.is_authorized_maintainer(&selected));
        assert_eq!(recovery.lead_maintainer(), Some(lead));
    }
}

#[cfg(test)]
mod role_discovery_expansion_tests {
    use super::*;

    fn pk() -> PublicKey {
        nostr::prelude::Keys::generate().public_key()
    }

    fn listing(
        author: PublicKey,
        maintainers: Vec<PublicKey>,
        moderators: Vec<PublicKey>,
    ) -> AnnouncementListing {
        AnnouncementListing {
            author,
            identifier: "repo".to_string(),
            maintainers,
            moderators,
        }
    }

    fn added_pubkeys(report: &FetchReport) -> HashSet<PublicKey> {
        report
            .repo_coordinates_without_relays
            .iter()
            .map(|c| c.public_key)
            .collect()
    }

    fn expand(
        announcements: &[AnnouncementListing],
        maintainer_listed: &mut HashSet<PublicKey>,
    ) -> (FetchReport, HashSet<Nip19Coordinate>, HashSet<PublicKey>) {
        let request = FetchRequest::default();
        let mut report = FetchReport::default();
        let mut fresh_coordinates = HashSet::new();
        let mut fresh_profiles = HashSet::new();
        expand_role_discovery(
            announcements,
            maintainer_listed,
            &request,
            &mut report,
            &mut fresh_coordinates,
            &mut fresh_profiles,
        );
        (report, fresh_coordinates, fresh_profiles)
    }

    /// A maintainer-listed author's announcement adds coordinates and
    /// profile fetches for both its maintainer and moderator listings, and
    /// its `m` listings grow the maintainer-listed set — while an `o`
    /// listing never does.
    #[test]
    fn maintainer_listed_author_expands_maintainers_and_moderators() {
        let owner = pk();
        let co = pk();
        let moderator = pk();
        let mut maintainer_listed = HashSet::from([owner]);

        let (report, fresh_coordinates, fresh_profiles) = expand(
            &[listing(owner, vec![owner, co], vec![moderator])],
            &mut maintainer_listed,
        );

        assert_eq!(
            added_pubkeys(&report),
            HashSet::from([owner, co, moderator])
        );
        assert_eq!(
            fresh_coordinates
                .iter()
                .map(|c| c.public_key)
                .collect::<HashSet<PublicKey>>(),
            HashSet::from([owner, co, moderator])
        );
        assert_eq!(fresh_profiles, HashSet::from([owner, co, moderator]));
        assert!(maintainer_listed.contains(&co));
        assert!(!maintainer_listed.contains(&moderator));
    }

    /// A role-fetched author who is not maintainer-listed (e.g. an
    /// `o`-assigned moderator) cannot expand discovery: their listings add
    /// no coordinates, no profiles and no maintainer-listed authors, so a
    /// moderator cannot direct the client to fetch arbitrary announcements.
    #[test]
    fn announcement_from_an_unlisted_author_expands_nothing() {
        let owner = pk();
        let moderator = pk();
        let crony = pk();
        let mut maintainer_listed = HashSet::from([owner]);

        let (report, fresh_coordinates, fresh_profiles) = expand(
            &[listing(moderator, vec![owner, crony], vec![crony])],
            &mut maintainer_listed,
        );

        assert!(report.repo_coordinates_without_relays.is_empty());
        assert!(fresh_coordinates.is_empty());
        assert!(fresh_profiles.is_empty());
        assert_eq!(maintainer_listed, HashSet::from([owner]));
    }

    /// The fixpoint makes expansion independent of arrival order: the
    /// co-maintainer's announcement is recorded before the owner's
    /// announcement that lists them, and its listings still expand.
    #[test]
    fn expansion_is_independent_of_announcement_order() {
        let owner = pk();
        let co = pk();
        let moderator = pk();
        let mut maintainer_listed = HashSet::from([owner]);

        let (report, _, _) = expand(
            &[
                listing(co, vec![co, owner], vec![moderator]),
                listing(owner, vec![owner, co], vec![]),
            ],
            &mut maintainer_listed,
        );

        assert!(added_pubkeys(&report).contains(&moderator));
        assert!(maintainer_listed.contains(&co));
    }
}

#[cfg(test)]
mod fetch_completion_tests {
    use futures::SinkExt;
    use nostr::prelude::{FinalizeEvent, Keys};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[tokio::test]
    async fn fetch_requires_eose_instead_of_accepting_partial_history() {
        // Exercise complete, empty, disconnected and silent responses over real
        // sockets.
        for (send_event, send_eose, disconnect) in [
            (true, true, false),
            (false, true, false),
            (true, false, true),
            (true, false, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let event = EventBuilder::new(Kind::TextNote, "history")
                .finalize(&Keys::generate())
                .unwrap();
            let expected = event.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    while let Some(frame) = ws.next().await {
                        let frame = frame.unwrap();
                        if !frame.is_text() {
                            continue;
                        }
                        let request: Value =
                            serde_json::from_str(frame.to_text().unwrap()).unwrap();
                        if request[0] != "REQ" {
                            continue;
                        }
                        if send_event {
                            ws.send(Message::Text(
                                serde_json::json!(["EVENT", request[1], event])
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        }
                        if send_eose {
                            ws.send(Message::Text(
                                serde_json::json!(["EOSE", request[1]]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                        }
                        if disconnect {
                            ws.close(None).await.unwrap();
                        }
                        let _ = stopped.await;
                        break;
                    }
                })
                .await
                .unwrap();
            });
            crate::tls::install_default_crypto_provider();
            let client = nostr_sdk::client::Client::default();
            client.add_relay(&url).await.unwrap();
            let relay = client.relay(&url).await.unwrap().unwrap();
            relay
                .try_connect()
                .timeout(Duration::from_secs(2))
                .await
                .unwrap();
            let result = fetch_complete_events(
                &relay,
                vec![Filter::new().kind(Kind::TextNote)],
                Duration::from_millis(250),
            )
            .await;
            client.disconnect().await;
            let _ = stop.send(());
            server.await.unwrap();
            if send_eose {
                assert_eq!(
                    result.unwrap(),
                    if send_event { vec![expected] } else { vec![] }
                );
            } else {
                assert!(result.is_err(), "partial history accepted: {result:?}");
            }
        }
    }
}
