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
    path::Path,
    sync::{
        Arc, Mutex, RwLock,
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
use nostr::{
    Event,
    event::UnsignedEvent,
    filter::Alphabet,
    nips::{
        nip01::Coordinate,
        nip05::{Nip05Address, Nip05Profile},
        nip19::Nip19Coordinate,
    },
    signer::SignerBackend,
};
use nostr_database::{NostrDatabase, SaveEventStatus};
use nostr_lmdb::NostrLMDB;
use nostr_relay_pool::relay::ReqExitPolicy;
use nostr_sdk::{
    ClientOptions, EventBuilder, EventId, Kind, NostrSigner, PublicKey, RelayUrl, SingleLetterTag,
    Timestamp, Url, prelude::RelayLimits,
};
use serde_json::Value;

use crate::{
    get_dirs,
    git::{Repo, RepoActions, get_git_config_item},
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, KIND_USER_GRASP_LIST, event_is_cover_letter,
        event_is_patch_set_root, event_is_revision_root, event_is_valid_pr_or_pr_update,
        status_kinds,
    },
    login::{get_likely_logged_in_user, user::get_user_ref_from_cache},
    repo_ref::{RepoRef, normalize_grasp_server_url},
    repo_state::RepoState,
};

pub fn is_verbose() -> bool {
    std::env::var("NGIT_VERBOSE").is_ok()
}

const SPINNER_EXPAND_DELAY_MS: u64 = 5000;

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
    client: nostr_sdk::Client,
    relay_default_set: Vec<String>,
    blaster_relays: Vec<String>,
    fallback_signer_relays: Vec<String>,
    grasp_default_set: Vec<String>,
    relays_not_to_retry: Arc<RwLock<HashMap<RelayUrl, String>>>,
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
    async fn set_signer(&mut self, signer: Arc<dyn NostrSigner>);
    async fn connect(&self, relay_url: &RelayUrl) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    fn get_relay_default_set(&self) -> &Vec<String>;
    fn get_blaster_relays(&self) -> &Vec<String>;
    fn get_fallback_signer_relays(&self) -> &Vec<String>;
    fn get_grasp_default_set(&self) -> &Vec<String>;
    async fn send_event_to<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        url: &str,
        event: nostr::event::Event,
    ) -> Result<nostr::EventId>;
    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>>;
    async fn get_events_per_relay(
        &self,
        relays: Vec<RelayUrl>,
        filters: Vec<nostr::Filter>,
        progress_reporter: MultiProgress,
    ) -> Result<(Vec<Result<Vec<nostr::Event>>>, MultiProgress)>;
    async fn fetch_all<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        repo_coordinates: Option<&'a Nip19Coordinate>,
        user_profiles: &HashSet<PublicKey>,
    ) -> Result<(Vec<Result<FetchReport>>, MultiProgress)>;
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
        Client {
            client: if let Some(keys) = opts.keys {
                nostr_sdk::ClientBuilder::new()
                    .opts(
                        ClientOptions::new()
                            .relay_limits(RelayLimits::disable())
                            .verify_subscriptions(true),
                    )
                    .signer(keys)
                    .build()
            } else {
                nostr_sdk::ClientBuilder::new()
                    .opts(
                        ClientOptions::new()
                            .relay_limits(RelayLimits::disable())
                            .verify_subscriptions(true),
                    )
                    .build()
            },
            relay_default_set: opts.relay_default_set,
            blaster_relays: opts.blaster_relays,
            fallback_signer_relays: opts.fallback_signer_relays,
            grasp_default_set: opts.grasp_default_set,
            relays_not_to_retry: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn set_signer(&mut self, signer: Arc<dyn NostrSigner>) {
        self.client.set_signer(signer).await;
    }

    async fn connect(&self, relay_url: &RelayUrl) -> Result<()> {
        if let Some(reason) = self.is_relay_skipped_for_session(relay_url) {
            bail!("{reason}");
        }
        self.client
            .add_relay(relay_url)
            .await
            .context("failed to add relay")?;

        let relay = self.client.relay(relay_url).await?;

        if !relay.is_connected() {
            #[allow(clippy::large_futures)]
            relay
                .try_connect(std::time::Duration::from_secs(long_timeout()))
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

    fn get_blaster_relays(&self) -> &Vec<String> {
        &self.blaster_relays
    }

    fn get_fallback_signer_relays(&self) -> &Vec<String> {
        &self.fallback_signer_relays
    }

    fn get_grasp_default_set(&self) -> &Vec<String> {
        &self.grasp_default_set
    }

    async fn send_event_to<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        url: &str,
        event: Event,
    ) -> Result<nostr::EventId> {
        self.client.add_relay(url).await?;
        #[allow(clippy::large_futures)]
        self.client.connect_relay(url).await?;
        self.client.relay(url).await?.send_event(&event).await?;
        if let Some(git_repo_path) = git_repo_path {
            save_event_in_local_cache(git_repo_path, &event).await?;
        }
        if [Kind::GitRepoAnnouncement, KIND_USER_GRASP_LIST].contains(&event.kind) {
            save_event_in_global_cache(git_repo_path, &event).await?;
        }
        Ok(event.id)
    }

    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>> {
        let (relay_results, _) = self
            .get_events_per_relay(
                relays.iter().map(|r| RelayUrl::parse(r).unwrap()).collect(),
                filters,
                MultiProgress::new(),
            )
            .await?;
        Ok(get_dedup_events(relay_results))
    }

    async fn get_events_per_relay(
        &self,
        relays: Vec<RelayUrl>,
        filters: Vec<nostr::Filter>,
        progress_reporter: MultiProgress,
    ) -> Result<(Vec<Result<Vec<nostr::Event>>>, MultiProgress)> {
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
                let progress_reporter_clone = progress_reporter.clone();
                async move {
                    let pb = if std::env::var("NGITTEST").is_err() {
                        let pb = progress_reporter_clone.add(
                            ProgressBar::new(1)
                                .with_prefix(format!("{: <11}{}", "connecting", relay.url()))
                                .with_style(pb_style(static_timeout_clone)?),
                        );
                        pb.enable_steady_tick(Duration::from_millis(300));
                        Some(pb)
                    } else {
                        None
                    };
                    fn update_progress_bar_with_error(
                        relay_url: &RelayUrl,
                        pb: Option<ProgressBar>,
                        error: &anyhow::Error,
                    ) {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(false));
                            pb.set_prefix(format!("{: <11}{}", "error", relay_url));
                            pb.finish_with_message(
                                console::style(
                                    error.to_string().replace("relay pool error:", "error:"),
                                )
                                .for_stderr()
                                .red()
                                .to_string(),
                            );
                        }
                    }
                    if let Some(reason) = self.is_relay_skipped_for_session(relay.url()) {
                        update_progress_bar_with_error(relay.url(), pb, &anyhow!("{reason}"));
                        bail!("{reason}");
                    }
                    #[allow(clippy::large_futures)]
                    match get_events_of(relay, filters, &pb).await {
                        Err(error) => {
                            // Check error for timeout/connection issues and add to skip list
                            if error.to_string().contains("connection timeout") {
                                self.skip_relay_for_session(relay.url().clone(), error.to_string());
                            }
                            update_progress_bar_with_error(relay.url(), pb, &error);
                            Err(error)
                        }
                        Ok(res) => {
                            if let Some(pb) = pb {
                                pb.set_style(pb_after_style(true));
                                pb.set_prefix(format!(
                                    "{: <11}{}",
                                    format!("{} events", res.len()),
                                    relay.url()
                                ));
                                pb.finish_with_message("");
                            }
                            Ok(res)
                        }
                    }
                }
            })
            .collect();

        let relay_results: Vec<Result<Vec<nostr::Event>>> =
            stream::iter(futures).buffer_unordered(15).collect().await;

        Ok((relay_results, progress_reporter))
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_all<'a>(
        &self,
        git_repo_path: Option<&'a Path>,
        trusted_maintainer_coordinate: Option<&'a Nip19Coordinate>,
        user_profiles: &HashSet<PublicKey>,
    ) -> Result<(Vec<Result<FetchReport>>, MultiProgress)> {
        let relay_default_set = &self
            .relay_default_set
            .iter()
            .filter_map(|r| RelayUrl::parse(r).ok())
            .collect::<HashSet<RelayUrl>>();

        let mut request = create_relays_request(
            git_repo_path,
            trusted_maintainer_coordinate,
            user_profiles,
            relay_default_set.clone(),
        )
        .await?;

        let verbose = is_verbose();
        let is_test = std::env::var("NGITTEST").is_ok();

        // Set up the two-MultiProgress pattern:
        // 1. A spinner MultiProgress shown immediately (concise mode only)
        // 2. A detail MultiProgress that starts hidden and becomes visible after a
        //    delay
        let spinner_multi = if !verbose && !is_test {
            let m = MultiProgress::new();
            let spinner = m.add(
                ProgressBar::new_spinner()
                    .with_style(
                        ProgressStyle::with_template("{spinner} {msg}")
                            .unwrap()
                            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈"),
                    )
                    .with_message("Checking nostr relays..."),
            );
            spinner.enable_steady_tick(Duration::from_millis(100));
            Some((m, spinner))
        } else {
            None
        };

        let progress_reporter = if is_test {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else if verbose {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };

        // Pre-add a heading bar at position 0 so it has a reserved slot
        // before any relay bars are added. It stays hidden (draw target is
        // hidden) until the timer reveals it.
        let heading_bar = if !verbose && !is_test {
            let bar = progress_reporter.add(
                ProgressBar::new(0).with_style(ProgressStyle::with_template("{msg}").unwrap()),
            );
            Some(bar)
        } else {
            None
        };

        // Track whether the detail view has been revealed. Bars that finish
        // before reveal have their finish_with_message deferred so they render
        // correctly once the draw target switches from hidden to stderr.
        let reveal_state: Option<Arc<BarRevealState>> = if !verbose && !is_test {
            Some(Arc::new(BarRevealState {
                revealed: AtomicBool::new(false),
                deferred: Mutex::new(Vec::new()),
            }))
        } else {
            None
        };

        // Spawn a background timer that transitions from spinner to detail view
        let detail_multi_for_timer = progress_reporter.clone();
        let spinner_for_timer = spinner_multi.as_ref().map(|(_, s)| s.clone());
        let reveal_state_for_timer = reveal_state.clone();
        let heading_bar_for_timer = heading_bar.clone();
        let timer_handle = if !verbose && !is_test {
            let handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(SPINNER_EXPAND_DELAY_MS)).await;
                // Transition: finish spinner, show heading, reveal detail bars
                if let Some(spinner) = spinner_for_timer {
                    spinner.finish_and_clear();
                }
                // Switch draw target to make bars visible
                detail_multi_for_timer.set_draw_target(ProgressDrawTarget::stderr());
                // Finish the pre-added heading bar now that the draw target
                // is visible so indicatif actually renders it.
                if let Some(heading) = heading_bar_for_timer {
                    heading.finish_with_message("Checking nostr relays...");
                }
                // Mark as revealed and flush all bars that finished while
                // the draw target was hidden. Hold the lock across the flag
                // update and drain so no bar can slip through unseen (see
                // the corresponding lock in finish_bar).
                if let Some(state) = reveal_state_for_timer {
                    let mut deferred = state.deferred.lock().unwrap();
                    state.revealed.store(true, Ordering::Release);
                    for df in deferred.drain(..) {
                        df.bar.finish_with_message(df.message);
                    }
                }
            });
            Some(handle)
        } else {
            None
        };

        let success_count = Arc::new(AtomicU64::new(0));
        let current_timeout = Arc::new(AtomicU64::new(long_timeout()));

        let mut processed_relays = HashSet::new();

        let mut relay_reports: Vec<Result<FetchReport>> = vec![];

        loop {
            let relays = request
                .repo_relays
                .union(&request.user_relays_for_profiles)
                .filter(|&r| !r.as_str().contains("nostr.mutinywallet.com"))
                .cloned()
                .collect::<HashSet<RelayUrl>>()
                .difference(&processed_relays)
                .cloned()
                .collect::<HashSet<RelayUrl>>();
            if relays.is_empty() {
                break;
            }
            let profile_relays_only = request
                .user_relays_for_profiles
                .difference(&request.repo_relays)
                .collect::<HashSet<&RelayUrl>>();
            for relay in &request.repo_relays {
                self.client
                    .add_relay(relay.as_str())
                    .await
                    .context("failed to add relay")?;
            }

            let success_count_for_loop = success_count.clone();
            let current_timeout_for_loop = current_timeout.clone();
            let total_relays = relays.len() as u64;

            let futures: Vec<_> = relays
                .iter()
                .map(|r| {
                    if profile_relays_only.contains(r) {
                        FetchRequest {
                            selected_relay: Some(r.to_owned()),
                            repo_coordinates_without_relays: vec![],
                            proposals: HashSet::new(),
                            missing_contributor_profiles: request
                                .missing_contributor_profiles
                                .union(
                                    &request
                                        .profiles_to_fetch_from_user_relays
                                        .clone()
                                        .into_keys()
                                        .collect(),
                                )
                                .copied()
                                .collect(),
                            ..request.clone()
                        }
                    } else {
                        FetchRequest {
                            selected_relay: Some(r.to_owned()),
                            ..request.clone()
                        }
                    }
                })
                .map(|request| {
                    let success_count_clone = success_count_for_loop.clone();
                    let current_timeout_clone = current_timeout_for_loop.clone();
                    let progress_reporter_clone = progress_reporter.clone();
                    let total_relays_clone = total_relays;
                    let reveal_state_clone = reveal_state.clone();
                    async move {
                        let relay_column_width = request.relay_column_width;

                        let relay_url = request
                            .selected_relay
                            .clone()
                            .context("fetch_all_from_relay called without a relay")?;

                        // Always create a real progress bar added to the detail
                        // multi. In test mode the multi has a hidden draw target
                        // so nothing is displayed. In concise mode the multi
                        // starts hidden and the background timer reveals it.
                        let pb = progress_reporter_clone.add(
                            ProgressBar::new(1)
                                .with_prefix(
                                    format!(
                                        "{: <relay_column_width$} connecting",
                                        &relay_url
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
                                        .apply_to(format!("{: <relay_column_width$}", &relay_url))
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
                                finish_bar(bar, msg, &reveal_state_clone);
                            }
                            bail!("{reason}");
                        }

                        let pb_clone = pb.clone();
                        let fetch_future = self.fetch_all_from_relay(git_repo_path, request, &pb_clone);
                        tokio::pin!(fetch_future);

                        let timeout_future = async {
                            let check_interval = Duration::from_millis(100);
                            let long_timeout_end = tokio::time::Instant::now() + Duration::from_secs(long_timeout());

                            loop {
                                let current_success_count = success_count_clone.load(Ordering::Relaxed);
                                let threshold = (total_relays_clone as f64 * SUCCESS_THRESHOLD).ceil() as u64;

                                if current_success_count >= threshold {
                                    tokio::time::sleep(Duration::from_secs(short_timeout())).await;
                                    return "short";
                                }

                                if tokio::time::Instant::now() >= long_timeout_end {
                                    return "long";
                                }

                                tokio::time::sleep(check_interval).await;
                            }
                        };

                        #[allow(clippy::large_futures)]
                        let result = tokio::select! {
                            result = &mut fetch_future => {
                                if result.is_ok() {
                                    let new_count = success_count_clone.fetch_add(1, Ordering::Relaxed) + 1;
                                    let threshold = (total_relays_clone as f64 * SUCCESS_THRESHOLD).ceil() as u64;

                                    if new_count >= threshold {
                                        current_timeout_clone.store(short_timeout(), Ordering::Relaxed);
                                    }
                                }
                                result
                            }
                            timeout_type = timeout_future => {
                                Err(anyhow!("timeout after {}s timeout",
                                    if timeout_type == "long" { long_timeout() } else { short_timeout() }))
                            }
                        };

                        match result {
                            Err(error) => {
                                if error.to_string().contains("connection timeout") || error.to_string().contains("timeout after") {
                                    self.skip_relay_for_session(relay_url.clone(), error.to_string());
                                }
                                let msg = style_progress_bar_with_error(
                                    relay_column_width,
                                    &relay_url,
                                    &pb,
                                    &error,
                                );
                                if let Some(ref bar) = pb {
                                    finish_bar(bar, msg, &reveal_state_clone);
                                }
                                Err(error)
                            }
                            Ok(res) => {
                                // The bar's style and prefix were already set
                                // by fetch_all_from_relay; finish it through
                                // the deferred mechanism.
                                if let Some(ref bar) = pb {
                                    finish_bar(bar, String::new(), &reveal_state_clone);
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
            processed_relays.extend(relays.clone());

            if let Some(trusted_maintainer_coordinate) = trusted_maintainer_coordinate {
                if let Ok(repo_ref) =
                    get_repo_ref_from_cache(git_repo_path, trusted_maintainer_coordinate).await
                {
                    request.repo_relays = repo_ref.relays.iter().cloned().collect();
                }
            }

            request.user_relays_for_profiles = {
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
        }

        // Cancel the background timer if it hasn't fired yet, and clean up
        // the spinner. If the timer already fired, the abort is a no-op.
        if let Some(handle) = timer_handle {
            handle.abort();
        }
        // Clear the spinner (no-op if timer already cleared it)
        if let Some((_, spinner)) = &spinner_multi {
            spinner.finish_and_clear();
        }

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

        let mut report = FetchReport::default();

        let relay_url = request
            .selected_relay
            .clone()
            .context("fetch_all_from_relay called without a relay")?;

        let relay_column_width = request.relay_column_width;

        let _ = self.client.add_relay(&relay_url).await;

        let dim = Style::new().color256(247);

        loop {
            let filters =
                get_fetch_filters(&fresh_coordinates, &fresh_proposal_roots, &fresh_profiles);

            if let Some(pb) = &pb {
                pb.set_prefix(
                    dim.apply_to(format!(
                        "{: <relay_column_width$} {}",
                        &relay_url,
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
            fresh_profiles = HashSet::new();

            let relay = self.client.relay(&relay_url).await?;
            let events: Vec<nostr::Event> = get_events_of(&relay, filters.clone(), pb).await?;
            // TODO: try reconcile

            process_fetched_events(
                events,
                &request,
                git_repo_path,
                &mut fresh_coordinates,
                &mut fresh_proposal_roots,
                &mut fresh_profiles,
                &mut report,
            )
            .await?;

            if fresh_coordinates.is_empty()
                && fresh_proposal_roots.is_empty()
                && fresh_profiles.is_empty()
            {
                break;
            }
        }
        if let Some(pb) = pb {
            pb.set_style(pb_after_style(true));
            pb.set_prefix(format!(
                "{} {}",
                dim.apply_to(format!("{: <relay_column_width$}", &relay_url))
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

static SUCCESS_THRESHOLD: f64 = 0.5; // 50% of relays must succeed to switch to short timeout

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
    relay: &nostr_sdk::Relay,
    filters: Vec<nostr::Filter>,
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
    while !relay.is_connected() {
        attempt_num += 1;
        #[allow(clippy::large_futures)]
        match relay
            .try_connect(Duration::from_secs(short_timeout()))
            .await
        {
            Ok(_) => {
                if relay.is_connected() {
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

    if !relay.is_connected() {
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

    let events_res = join_all(filters.into_iter().map(|filter| async {
        relay
            .fetch_events(
                filter,
                // Use a very long timeout; actual timeout is controlled by outer tokio::select!
                std::time::Duration::from_secs(long_timeout()),
                ReqExitPolicy::ExitOnEOSE,
            )
            .await
    }))
    .await;

    // no Event is being mutated, just new items added to the set
    #[allow(clippy::mutable_key_type)]
    let mut events: HashSet<Event> = HashSet::new();

    for res in events_res {
        events.extend(res?);
    }
    Ok(events.into_iter().collect())
}

pub struct Params {
    pub keys: Option<nostr::Keys>,
    pub relay_default_set: Vec<String>,
    pub blaster_relays: Vec<String>,
    pub fallback_signer_relays: Vec<String>,
    pub grasp_default_set: Vec<String>,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            keys: None,
            relay_default_set: if std::env::var("NGITTEST").is_ok() {
                vec![
                    "ws://localhost:8051".to_string(),
                    "ws://localhost:8052".to_string(),
                ]
            } else {
                vec![
                    "wss://relay.damus.io".to_string(), /* free, good reliability, have been
                                                         * known
                                                         * to delete all messages */
                    "wss://nos.lol".to_string(),
                ]
            },
            blaster_relays: if std::env::var("NGITTEST").is_ok() {
                vec!["ws://localhost:8057".to_string()]
            } else {
                vec![]
            },
            fallback_signer_relays: if std::env::var("NGITTEST").is_ok() {
                vec!["ws://localhost:8051".to_string()]
            } else {
                vec![
                    "wss://relay.nsec.app".to_string(),
                    "wss://relay.ditto.pub".to_string(),
                ]
            },
            grasp_default_set: if std::env::var("NGITTEST").is_ok() {
                vec![]
            } else {
                vec!["relay.ngit.dev".to_string(), "gitnostr.com".to_string()]
            },
        }
    }
}
impl Params {
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
        params
    }
}

fn get_dedup_events(relay_results: Vec<Result<Vec<nostr::Event>>>) -> Vec<Event> {
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
    signer: &Arc<dyn NostrSigner>,
    description: String,
) -> Result<nostr::Event> {
    if signer.backend() == SignerBackend::NostrConnect {
        let term = console::Term::stderr();
        term.write_line(&format!(
            "signing event ({description}) with remote signer..."
        ))?;
        let event = signer
            .sign_event(event_builder.build(signer.get_public_key().await?))
            .await
            .context("failed to sign event")?;
        term.clear_last_lines(1)?;
        Ok(event)
    } else {
        signer
            .sign_event(event_builder.build(signer.get_public_key().await?))
            .await
            .context("failed to sign event")
    }
}

pub async fn sign_draft_event(
    draft_event: UnsignedEvent,
    signer: &Arc<dyn NostrSigner>,
    description: String,
) -> Result<nostr::Event> {
    if signer.backend() == SignerBackend::NostrConnect {
        let term = console::Term::stderr();
        term.write_line(&format!(
            "signing event ({description}) with remote signer..."
        ))?;
        let event = signer
            .sign_event(draft_event)
            .await
            .context("failed to sign event")?;
        term.clear_last_lines(1)?;
        Ok(event)
    } else {
        signer
            .sign_event(draft_event)
            .await
            .context("failed to sign event")
    }
}

pub async fn fetch_public_key(signer: &Arc<dyn NostrSigner>) -> Result<nostr::PublicKey> {
    if signer.backend() == SignerBackend::NostrConnect {
        let term = console::Term::stderr();
        term.write_line("fetching npub from remote signer...")?;
        let public_key = signer
            .get_public_key()
            .await
            .context("failed to get npub from remote signer")?;
        term.clear_last_lines(1)?;
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
    let json_res: Value = reqwest::Client::new()
        .get(addr_deconstructed.url().to_string())
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
    Nip05Profile::from_json(&addr_deconstructed, &json_res).context(format!(
        "cannot get public key for nip05 address: {nip05_addr}"
    ))
}

fn pb_style(current_timeout: Arc<AtomicU64>) -> Result<ProgressStyle> {
    Ok(
        ProgressStyle::with_template(" {spinner} {prefix} {msg} {timeout_in}")?.with_key(
            "timeout_in",
            move |state: &ProgressState, w: &mut dyn Write| {
                let elapsed = state.elapsed().as_secs();
                // Adaptive timeout display: reads the actual current timeout value
                // which starts at LONG_TIMEOUT and switches to SHORT_TIMEOUT after
                // the first relay succeeds
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

async fn get_local_cache_database(git_repo_path: &Path) -> Result<NostrLMDB> {
    NostrLMDB::open(git_repo_path.join(".git/nostr-cache.lmdb"))
        .context("failed to open or create nostr cache database at .git/nostr-cache.lmdb")
}

async fn get_global_cache_database(git_repo_path: Option<&Path>) -> Result<NostrLMDB> {
    let path = if std::env::var("NGITTEST").is_ok() {
        if let Some(git_repo_path) = git_repo_path {
            git_repo_path.join(".git/test-global-cache.lmdb")
        } else {
            bail!("git_repo must be supplied to get_global_cache_database during integration tests")
        }
    } else {
        create_dir_all(get_dirs()?.cache_dir()).context(format!(
            "failed to create cache directory in: {:?}",
            get_dirs()?.cache_dir()
        ))?;
        get_dirs()?.cache_dir().join("nostr-cache.lmdb")
    };

    NostrLMDB::open(path).context("failed to open ngit global nostr cache database")
}

pub async fn get_events_from_local_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
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
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
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

pub async fn save_event_in_local_cache(git_repo_path: &Path, event: &nostr::Event) -> Result<bool> {
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

pub async fn save_event_in_global_cache(
    git_repo_path: Option<&Path>,
    event: &nostr::Event,
) -> Result<bool> {
    match get_global_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("failed to save event in local cache")
    {
        Ok(SaveEventStatus::Success) => Ok(true),
        Ok(_) => Ok(false),
        Err(e) => Err(e).context("failed to save event in local cache"),
    }
}

// use annoucement from trusted maintainer but recursively add maintainers, git
// servers and relays
pub async fn get_repo_ref_from_cache(
    git_repo_path: Option<&Path>,
    repo_coordinate: &Nip19Coordinate,
) -> Result<RepoRef> {
    let mut maintainers = HashSet::new();
    let mut new_coordinate: bool;

    maintainers.insert(repo_coordinate.public_key);
    let mut repo_events = vec![];
    loop {
        new_coordinate = false;
        let repo_events_filter = get_filter_repo_ann_events(
            &HashSet::from_iter(maintainers.iter().map(|m| Nip19Coordinate {
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
                for m in repo_ref.maintainers {
                    if maintainers.insert(m) {
                        new_coordinate = true;
                    }
                }
                repo_events.push(e);
            }
        }
        if !new_coordinate {
            break;
        }
    }
    repo_events.sort_by_key(|e| e.created_at);
    let repo_ref = RepoRef::try_from((
        repo_events
            .iter()
            .find(|e| e.pubkey == repo_coordinate.public_key)
            .context("no repo announcement event found at specified Nip19Coordinates. if you are the repository maintainer consider running `ngit init` to create one")?
            .clone(),
        Some(repo_coordinate.public_key),
    ))?;

    // Use name/description/web from the latest event across all maintainers
    let latest_metadata = repo_events
        .last()
        .and_then(|e| RepoRef::try_from((e.clone(), None)).ok());

    let mut events: HashMap<Nip19Coordinate, nostr::Event> = HashMap::new();
    for m in &maintainers {
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

    // Use relays, git and blossom servers from all maintainer announcement events
    // we use Vec and HashSet to remove duplicates and preserve order
    let mut relays: Vec<RelayUrl> = repo_ref.relays.clone();
    let mut git_server: Vec<String> = repo_ref.git_server.clone();
    let mut blossoms: Vec<Url> = repo_ref.blossoms.clone();
    let mut seen_relays: HashSet<RelayUrl> = HashSet::from_iter(relays.iter().cloned());
    let mut seen_git_server: HashSet<String> = git_server
        .iter()
        .map(|server| server.trim_end_matches('/').to_string())
        .collect();
    let mut seen_blossoms: HashSet<Url> = HashSet::from_iter(blossoms.iter().cloned());

    // also set maintainers_without_annoucnement
    let mut maintainers_without_annoucnement: Vec<PublicKey> = vec![];

    for m in &maintainers {
        if let Some(event) = repo_events.iter().find(|e| e.pubkey == *m) {
            if let Ok(m_repo_ref) = RepoRef::try_from((event.clone(), None)) {
                for relay in m_repo_ref.relays {
                    if seen_relays.insert(relay.clone()) {
                        relays.push(relay);
                    }
                }
                for server in m_repo_ref.git_server {
                    if seen_git_server.insert(server.trim_end_matches('/').to_string()) {
                        git_server.push(server);
                    }
                }
                for blossom in m_repo_ref.blossoms {
                    if seen_blossoms.insert(blossom.clone()) {
                        blossoms.push(blossom);
                    }
                }
            }
        } else {
            maintainers_without_annoucnement.push(*m);
        }
    }

    Ok(RepoRef {
        // use all maintainers from all events found, not just maintainers in the most
        // recent event
        maintainers: maintainers.iter().copied().collect::<Vec<PublicKey>>(),
        relays,
        git_server,
        events,
        maintainers_without_annoucnement: Some(maintainers_without_annoucnement),
        name: latest_metadata
            .as_ref()
            .map_or_else(|| repo_ref.name.clone(), |r| r.name.clone()),
        description: latest_metadata
            .as_ref()
            .map_or_else(|| repo_ref.description.clone(), |r| r.description.clone()),
        web: latest_metadata
            .as_ref()
            .map_or_else(|| repo_ref.web.clone(), |r| r.web.clone()),
        ..repo_ref
    })
}

pub async fn get_state_from_cache(
    git_repo_path: Option<&Path>,
    repo_ref: &RepoRef,
) -> Result<RepoState> {
    if let Some(git_repo_path) = git_repo_path {
        RepoState::try_from(
            get_events_from_local_cache(
                git_repo_path,
                vec![get_filter_state_events(&repo_ref.coordinates(), true)],
            )
            .await?,
        )
    } else {
        RepoState::try_from(
            get_event_from_global_cache(
                git_repo_path,
                vec![get_filter_state_events(&repo_ref.coordinates(), true)],
            )
            .await?,
        )
    }
}

#[allow(clippy::too_many_lines)]
async fn create_relays_request(
    git_repo_path: Option<&Path>,
    trusted_maintainer_coordinate: Option<&Nip19Coordinate>,
    user_profiles: &HashSet<PublicKey>,
    fallback_relays: HashSet<RelayUrl>,
) -> Result<FetchRequest> {
    let repo_ref = if let Some(trusted_maintainer_coordinate) = trusted_maintainer_coordinate {
        (get_repo_ref_from_cache(git_repo_path, trusted_maintainer_coordinate).await).ok()
    } else {
        None
    };

    let repo_coordinates = {
        // add Nip19Coordinates of users listed in maintainers to explicitly
        // specified coodinates
        let mut set: HashSet<Nip19Coordinate> = HashSet::new();
        if let Some(trusted_maintainer_coordinate) = trusted_maintainer_coordinate {
            set.insert(trusted_maintainer_coordinate.clone());
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
                    nostr::Filter::default()
                        .kinds(vec![Kind::GitPatch])
                        .custom_tags(
                            SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                            repo_coordinates_without_relays
                                .iter()
                                .map(|c| c.coordinate.to_string())
                                .collect::<Vec<String>>(),
                        ),
                ],
            )
            .await?
            {
                if event_is_patch_set_root(event) || event_is_revision_root(event) {
                    proposals.insert(event.id);
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

    let user_relays_for_profiles = {
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
            &missing_contributor_profiles
                .union(
                    &profiles_to_fetch_from_user_relays
                        .clone()
                        .into_keys()
                        .collect(),
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

    let relays = {
        // Only use fallback relays for bootstrapping (no repo context).
        // When we have a repo coordinate, rely on repo relays and coordinate
        // hint relays instead of always merging in the default set.
        let mut relays = if trusted_maintainer_coordinate.is_none() {
            fallback_relays
        } else {
            HashSet::new()
        };
        if let Some(repo_ref) = &repo_ref {
            for r in repo_ref.relays.clone() {
                relays.insert(r);
            }
        }
        for c in repo_coordinates {
            for r in &c.relays {
                relays.insert(r.clone());
            }
        }
        // When bootstrapping with no repo context and no coordinate hints,
        // we need at least the fallback relays to discover the user profile.
        relays
    };

    let relay_column_width = relays
        .union(&user_relays_for_profiles)
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
        .unwrap()
        .to_string()
        .chars()
        .count()
        + 2;

    Ok(FetchRequest {
        selected_relay: None,
        repo_relays: relays,
        relay_column_width,
        repo_coordinates_without_relays: if let Some(repo_ref) = &repo_ref {
            repo_ref.coordinates_with_timestamps()
        } else {
            repo_coordinates_without_relays
                .iter()
                .map(|c| (c.clone(), None))
                .collect()
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
        proposals,
        contributors,
        missing_contributor_profiles,
        existing_events,
        profiles_to_fetch_from_user_relays,
        user_relays_for_profiles,
    })
}

#[allow(clippy::too_many_lines)]
async fn process_fetched_events(
    events: Vec<nostr::Event>,
    request: &FetchRequest,
    git_repo_path: Option<&Path>,
    fresh_coordinates: &mut HashSet<Nip19Coordinate>,
    fresh_proposal_roots: &mut HashSet<EventId>,
    fresh_profiles: &mut HashSet<PublicKey>,
    report: &mut FetchReport,
) -> Result<()> {
    for event in &events {
        if !request.existing_events.contains(&event.id) {
            if let Some(git_repo_path) = git_repo_path {
                save_event_in_local_cache(git_repo_path, event).await?;
            }
            if event.kind.eq(&Kind::GitRepoAnnouncement) {
                save_event_in_global_cache(git_repo_path, event).await?;
                let new_coordinate = !request
                    .repo_coordinates_without_relays
                    .iter()
                    .map(|(c, _)| c.clone())
                    .any(|c| {
                        c.identifier.eq(event.tags.identifier().unwrap())
                            && c.public_key.eq(&event.pubkey)
                    });
                let update_to_existing = !new_coordinate
                    && request
                        .repo_coordinates_without_relays
                        .iter()
                        .any(|(c, t)| {
                            c.identifier.eq(event.tags.identifier().unwrap())
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
                    for m in &repo_ref.maintainers {
                        if !request
                            .repo_coordinates_without_relays // prexisting maintainers
                            .iter()
                            .map(|(c, _)| c.clone())
                            .collect::<HashSet<Nip19Coordinate>>()
                            .union(&report.repo_coordinates_without_relays) // already added maintainers
                            .any(|c| c.identifier.eq(&repo_ref.identifier) && m.eq(&c.public_key))
                        {
                            let c = Nip19Coordinate {
                                coordinate: Coordinate {
                                    kind: event.kind,
                                    public_key: *m,
                                    identifier: repo_ref.identifier.clone(),
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
            } else if event_is_patch_set_root(event) || event.kind.eq(&KIND_PULL_REQUEST) {
                fresh_proposal_roots.insert(event.id);
                report.proposals.insert(event.id);
                if !request.contributors.contains(&event.pubkey)
                    && !fresh_profiles.contains(&event.pubkey)
                {
                    fresh_profiles.insert(event.pubkey);
                }
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
        if !request.existing_events.contains(&event.id)
            && !event.tags.iter().any(|t| {
                t.as_slice().len() > 1
                    && (t.as_slice()[0].eq("E") || t.as_slice()[0].eq("e"))
                    && if let Ok(id) = EventId::parse(&t.as_slice()[1]) {
                        report.proposals.contains(&id)
                    } else {
                        false
                    }
            })
        {
            if (event.kind.eq(&Kind::GitPatch) && !event_is_patch_set_root(event))
                || event.kind.eq(&KIND_PULL_REQUEST_UPDATE)
            {
                report.commits.insert(event.id);
            } else if status_kinds().contains(&event.kind) {
                report.statuses.insert(event.id);
            }
        }
    }
    Ok(())
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
        for c in relay_report.contributor_profiles {
            report.contributor_profiles.insert(c);
        }
        for c in relay_report.profile_updates {
            report.profile_updates.insert(c);
        }
    }
    report
}
pub fn get_fetch_filters(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    proposal_ids: &HashSet<EventId>,
    required_profiles: &HashSet<PublicKey>,
) -> Vec<nostr::Filter> {
    [
        if repo_coordinates.is_empty() {
            vec![]
        } else {
            vec![
                get_filter_state_events(repo_coordinates, false),
                get_filter_repo_ann_events(repo_coordinates, false),
                nostr::Filter::default()
                    .kinds(vec![Kind::GitPatch, Kind::EventDeletion, KIND_PULL_REQUEST])
                    .custom_tags(
                        SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                        repo_coordinates
                            .iter()
                            .map(|c| c.coordinate.to_string())
                            .collect::<Vec<String>>(),
                    ),
            ]
        },
        if proposal_ids.is_empty() {
            vec![]
        } else {
            vec![
                nostr::Filter::default().events(proposal_ids.clone()).kinds(
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
                nostr::Filter::default()
                    .custom_tags(
                        SingleLetterTag::uppercase(Alphabet::E),
                        proposal_ids.clone(),
                    )
                    .kinds(
                        [
                            vec![Kind::EventDeletion, KIND_PULL_REQUEST_UPDATE],
                            status_kinds(),
                        ]
                        .concat(),
                    ),
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

pub fn get_filter_repo_ann_events(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    maintainers_only: bool,
) -> nostr::Filter {
    let filter = nostr::Filter::default()
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

pub static STATE_KIND: nostr::Kind = Kind::Custom(30618);
pub fn get_filter_state_events(
    repo_coordinates: &HashSet<Nip19Coordinate>,
    maintainers_only: bool,
) -> nostr::Filter {
    let filter = nostr::Filter::default().kind(STATE_KIND).identifiers(
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

pub fn get_filter_contributor_profiles(contributors: HashSet<PublicKey>) -> nostr::Filter {
    nostr::Filter::default()
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
    contributor_profiles: HashSet<PublicKey>,
    profile_updates: HashSet<PublicKey>,
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

#[derive(Default, Clone)]
pub struct FetchRequest {
    repo_relays: HashSet<RelayUrl>,
    selected_relay: Option<RelayUrl>,
    relay_column_width: usize,
    repo_coordinates_without_relays: Vec<(Nip19Coordinate, Option<Timestamp>)>,
    state: Option<(Timestamp, EventId)>,
    proposals: HashSet<EventId>,
    contributors: HashSet<PublicKey>,
    missing_contributor_profiles: HashSet<PublicKey>,
    existing_events: HashSet<EventId>,
    profiles_to_fetch_from_user_relays: HashMap<PublicKey, (Timestamp, Timestamp, Timestamp)>,
    user_relays_for_profiles: HashSet<RelayUrl>,
}

pub async fn fetching_with_report(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    trusted_maintainer_coordinate: &Nip19Coordinate,
) -> Result<FetchReport> {
    let verbose = is_verbose();
    if verbose {
        let term = console::Term::stderr();
        term.write_line("Checking nostr relays...")?;
    }
    let (relay_reports, progress_reporter) = client
        .fetch_all(
            Some(git_repo_path),
            Some(trusted_maintainer_coordinate),
            &HashSet::new(),
        )
        .await?;
    if !relay_reports.iter().any(std::result::Result::is_err) {
        let _ = progress_reporter.clear();
    }
    let report = consolidate_fetch_reports(relay_reports);
    if report.to_string().is_empty() {
        println!("no updates");
    } else {
        println!("updates: {report}");
    }
    Ok(report)
}

pub async fn get_proposals_and_revisions_from_cache(
    git_repo_path: &Path,
    repo_coordinates: HashSet<Nip19Coordinate>,
) -> Result<Vec<nostr::Event>> {
    let mut proposals = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .kinds([nostr::Kind::GitPatch, KIND_PULL_REQUEST])
                .custom_tags(
                    nostr::SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
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
    .collect::<Vec<nostr::Event>>();
    proposals.sort_by_key(|e| e.created_at);
    proposals.reverse();
    Ok(proposals)
}

pub async fn get_all_proposal_patch_pr_pr_update_events_from_cache(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    proposal_id: &nostr::EventId,
) -> Result<Vec<nostr::Event>> {
    let mut commit_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .kinds([
                    nostr::Kind::GitPatch,
                    KIND_PULL_REQUEST,
                    KIND_PULL_REQUEST_UPDATE,
                ])
                .event(*proposal_id),
            nostr::Filter::default()
                .kinds([
                    nostr::Kind::GitPatch,
                    KIND_PULL_REQUEST,
                    KIND_PULL_REQUEST_UPDATE,
                ])
                .custom_tag(SingleLetterTag::uppercase(Alphabet::E), *proposal_id),
            nostr::Filter::default()
                .kinds([nostr::Kind::GitPatch, KIND_PULL_REQUEST])
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

    let revision_roots: HashSet<nostr::EventId> = commit_events
        .iter()
        .filter(|e| event_is_revision_root(e))
        .map(|e| e.id)
        .collect();

    if !revision_roots.is_empty() {
        for event in get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds([
                        nostr::Kind::GitPatch,
                        KIND_PULL_REQUEST,
                        KIND_PULL_REQUEST_UPDATE,
                    ])
                    .events(revision_roots.clone())
                    .authors(permissioned_users.clone()),
                nostr::Filter::default()
                    .kinds([
                        nostr::Kind::GitPatch,
                        KIND_PULL_REQUEST,
                        KIND_PULL_REQUEST_UPDATE,
                    ])
                    .custom_tags(SingleLetterTag::uppercase(Alphabet::E), revision_roots)
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
        vec![nostr::Filter::default().id(*event_id)],
    )
    .await?
    .first()
    .context("failed to find event in cache")?
    .clone())
}

#[allow(clippy::module_name_repetitions)]
#[allow(clippy::too_many_lines)]
pub async fn send_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: Option<&Path>,
    events: Vec<nostr::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<RelayUrl>,
    animate: bool,
    silent: bool,
) -> Result<()> {
    // Only include default relays as fallback when there are no repo relays
    // (bootstrapping case, e.g. new account signup). When repo relays exist,
    // trust the repo and user relay configuration.
    let fallback = [
        if repo_read_relays.is_empty() && my_write_relays.is_empty() {
            client.get_relay_default_set().clone()
        } else {
            vec![]
        },
        if events.iter().any(|e| e.kind.eq(&Kind::GitRepoAnnouncement)) {
            client.get_blaster_relays().clone()
        } else {
            vec![]
        },
    ]
    .concat();
    let mut relays: Vec<&str> = vec![];

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

    let verbose = is_verbose();
    let is_test = std::env::var("NGITTEST").is_ok();
    let use_concise = !is_test || (!verbose && !silent && animate);

    let events_description = describe_events(&events);

    // Set up the two-MultiProgress pattern (same as fetch_all):
    // 1. A spinner MultiProgress shown immediately (concise mode only)
    // 2. A detail MultiProgress that starts hidden and becomes visible after a
    //    delay
    let spinner_multi = if use_concise {
        let sm = MultiProgress::new();
        let spinner = sm.add(
            ProgressBar::new_spinner()
                .with_style(
                    ProgressStyle::with_template("{spinner} {msg}")
                        .unwrap()
                        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈"),
                )
                .with_message(format!(
                    "Publishing {events_description} to nostr relays..."
                )),
        );
        spinner.enable_steady_tick(Duration::from_millis(100));
        Some((sm, spinner))
    } else {
        None
    };

    let m = if silent || !is_test || use_concise {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
    } else {
        MultiProgress::new()
    };

    // Pre-add a heading bar at position 0 so it has a reserved slot
    // before any relay bars are added.
    let heading_bar = {
        let bar =
            m.add(ProgressBar::new(0).with_style(ProgressStyle::with_template("{msg}").unwrap()));
        if !is_test {
            bar.set_message(format!(
                "Publishing {events_description} to nostr relays..."
            ));
        }
        Some(bar)
    };

    let reveal_state: Option<Arc<BarRevealState>> = if use_concise {
        Some(Arc::new(BarRevealState {
            revealed: AtomicBool::new(false),
            deferred: Mutex::new(Vec::new()),
        }))
    } else {
        None
    };

    // Spawn a background timer that transitions from spinner to detail view
    let detail_multi_for_timer = m.clone();
    let spinner_for_timer = spinner_multi.as_ref().map(|(_, s)| s.clone());
    let reveal_state_for_timer = reveal_state.clone();
    let heading_bar_for_timer = heading_bar.clone();
    let events_description_for_timer = events_description.clone();
    let timer_handle = if use_concise {
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(SPINNER_EXPAND_DELAY_MS)).await;
            if let Some(spinner) = spinner_for_timer {
                spinner.finish_and_clear();
            }
            detail_multi_for_timer.set_draw_target(ProgressDrawTarget::stderr());
            if let Some(heading) = heading_bar_for_timer {
                heading.finish_with_message(format!(
                    "Publishing {events_description_for_timer} to nostr relays..."
                ));
            }
            if let Some(state) = reveal_state_for_timer {
                let mut deferred = state.deferred.lock().unwrap();
                state.revealed.store(true, Ordering::Release);
                for df in deferred.drain(..) {
                    df.bar.finish_with_message(df.message);
                }
            }
        });
        Some(handle)
    } else {
        None
    };

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
    join_all(relays.iter().map(|&relay| {
        let reveal_state_clone = reveal_state.clone();
        let my_write_relays = my_write_relays.clone();
        let repo_read_relays = repo_read_relays.clone();
        let fallback = fallback.clone();
        let m = m.clone();
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
            let pb = m.add(
                ProgressBar::new(events.len() as u64)
                    .with_prefix(details.to_string())
                    .with_style(pb_style.clone()),
            );
            if animate {
                pb.enable_steady_tick(Duration::from_millis(300));
            }
            pb.inc(0); // need to make pb display intially
            let mut failed = false;
            for event in &events {
                match client
                    .send_event_to(git_repo_path, relay, event.clone())
                    .await
                {
                    Ok(_) => pb.inc(1),
                    Err(e) => {
                        pb.set_style(pb_after_style_failed.clone());
                        let msg = console::style(format!(
                            "error: {}",
                            e.to_string()
                                .replace("relay pool error:", "")
                                .replace("event not published: ", "")
                        ))
                        .for_stderr()
                        .red()
                        .to_string();
                        finish_bar(&pb, msg, &reveal_state_clone);
                        failed = true;
                        break;
                    }
                };
            }
            if !failed {
                pb.set_style(pb_after_style_succeeded.clone());
                finish_bar(&pb, String::new(), &reveal_state_clone);
            }
        }
    }))
    .await;

    // Cancel the background timer if it hasn't fired yet, and clean up
    // the spinner. If the timer already fired, the abort is a no-op.
    if let Some(handle) = timer_handle {
        handle.abort();
    }
    if let Some((_, spinner)) = &spinner_multi {
        spinner.set_style(ProgressStyle::with_template("{msg}").unwrap());
        spinner.finish_with_message(format!("Published {events_description} to nostr relays"));
    }

    Ok(())
}

/// Builds a human-readable description of what is being published, e.g.
/// "3 patches", "1 announcement and 1 state event", "2 patches and 1 cover
/// letter".
fn describe_events(events: &[nostr::Event]) -> String {
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

fn remove_trailing_slash(s: &str) -> String {
    match s.strip_suffix('/') {
        Some(s) => s,
        None => s,
    }
    .to_string()
}
