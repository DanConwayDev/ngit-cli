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
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use console::Style;
use futures::{
    future::join_all,
    stream::{self, StreamExt},
};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
#[cfg(test)]
use mockall::*;
use nostr::{nips::nip01::Coordinate, Event};
use nostr_database::{NostrDatabase, Order};
use nostr_sdk::{
    prelude::RelayLimits, EventBuilder, EventId, Kind, NostrSigner, Options, PublicKey,
    SingleLetterTag, Timestamp, Url,
};
use nostr_sqlite::SQLiteDatabase;

use crate::{
    get_dirs,
    git::{Repo, RepoActions},
    git_events::{
        event_is_cover_letter, event_is_patch_set_root, event_is_revision_root, status_kinds,
    },
    login::{get_logged_in_user, get_user_ref_from_cache},
    repo_ref::RepoRef,
    repo_state::RepoState,
};

#[allow(clippy::struct_field_names)]
pub struct Client {
    client: nostr_sdk::Client,
    fallback_relays: Vec<String>,
    more_fallback_relays: Vec<String>,
    blaster_relays: Vec<String>,
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Connect {
    fn default() -> Self;
    fn new(opts: Params) -> Self;
    async fn set_signer(&mut self, signer: NostrSigner);
    async fn connect(&self, relay_url: &Url) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    fn get_fallback_relays(&self) -> &Vec<String>;
    fn get_more_fallback_relays(&self) -> &Vec<String>;
    fn get_blaster_relays(&self) -> &Vec<String>;
    async fn send_event_to(
        &self,
        git_repo_path: &Path,
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
        relays: Vec<Url>,
        filters: Vec<nostr::Filter>,
        progress_reporter: MultiProgress,
    ) -> Result<(Vec<Result<Vec<nostr::Event>>>, MultiProgress)>;
    async fn fetch_all(
        &self,
        git_repo_path: &Path,
        repo_coordinates: &HashSet<Coordinate>,
        user_profiles: &HashSet<PublicKey>,
    ) -> Result<(Vec<Result<FetchReport>>, MultiProgress)>;
    async fn fetch_all_from_relay(
        &self,
        git_repo_path: &Path,
        request: FetchRequest,
        pb: &Option<ProgressBar>,
    ) -> Result<FetchReport>;
}

#[async_trait]
impl Connect for Client {
    fn default() -> Self {
        let fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8051".to_string(),
                "ws://localhost:8052".to_string(),
            ]
        } else {
            vec![
                "wss://relay.damus.io".to_string(), /* free, good reliability, have been known
                                                     * to delete all messages */
                "wss://nos.lol".to_string(),
                "wss://relay.nostr.band".to_string(),
            ]
        };

        let more_fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8055".to_string(),
                "ws://localhost:8056".to_string(),
            ]
        } else {
            vec![
                "wss://purplerelay.com".to_string(), // free but reliability not tested
                "wss://purplepages.es".to_string(),  // for profile events but unreliable
                "wss://relayable.org".to_string(),   // free but not always reliable
            ]
        };

        let blaster_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec!["ws://localhost:8057".to_string()]
        } else {
            vec!["wss://nostr.mutinywallet.com".to_string()]
        };
        Client {
            client: nostr_sdk::ClientBuilder::new()
                .opts(Options::new().relay_limits(RelayLimits::disable()))
                .build(),
            fallback_relays,
            more_fallback_relays,
            blaster_relays,
        }
    }
    fn new(opts: Params) -> Self {
        Client {
            client: nostr_sdk::ClientBuilder::new()
                .opts(Options::new().relay_limits(RelayLimits::disable()))
                .signer(opts.keys.unwrap_or(nostr::Keys::generate()))
                // .database(
                //     SQLiteDatabase::open(get_dirs()?.cache_dir().join("nostr-cache.sqlite")).
                // await?, )
                .build(),
            fallback_relays: opts.fallback_relays,
            more_fallback_relays: opts.more_fallback_relays,
            blaster_relays: opts.blaster_relays,
        }
    }

    async fn set_signer(&mut self, signer: NostrSigner) {
        self.client.set_signer(Some(signer)).await;
    }

    async fn connect(&self, relay_url: &Url) -> Result<()> {
        self.client
            .add_relay(relay_url)
            .await
            .context("cannot add relay")?;

        let relay = self.client.relay(relay_url).await?;

        if !relay.is_connected().await {
            #[allow(clippy::large_futures)]
            relay
                .connect(Some(std::time::Duration::from_secs(CONNECTION_TIMEOUT)))
                .await;
        }

        if !relay.is_connected().await {
            bail!("connection timeout");
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await?;
        Ok(())
    }

    fn get_fallback_relays(&self) -> &Vec<String> {
        &self.fallback_relays
    }

    fn get_more_fallback_relays(&self) -> &Vec<String> {
        &self.more_fallback_relays
    }

    fn get_blaster_relays(&self) -> &Vec<String> {
        &self.blaster_relays
    }

    async fn send_event_to(
        &self,
        git_repo_path: &Path,
        url: &str,
        event: Event,
    ) -> Result<nostr::EventId> {
        self.client.add_relay(url).await?;
        #[allow(clippy::large_futures)]
        self.client.connect_relay(url).await?;
        let res = self.client.send_event_to(vec![url], event.clone()).await?;
        if let Some(err) = res.failed.get(&Url::parse(url)?) {
            bail!(if let Some(err) = err {
                err.to_string()
            } else {
                "error: unknown".to_string()
            });
        }
        save_event_in_cache(git_repo_path, &event).await?;
        if event.kind().eq(&Kind::GitRepoAnnouncement) {
            save_event_in_global_cache(git_repo_path, &event).await?;
        }
        Ok(event.id())
    }

    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>> {
        let (relay_results, _) = self
            .get_events_per_relay(
                relays.iter().map(|r| Url::parse(r).unwrap()).collect(),
                filters,
                MultiProgress::new(),
            )
            .await?;
        Ok(get_dedup_events(relay_results))
    }

    async fn get_events_per_relay(
        &self,
        relays: Vec<Url>,
        filters: Vec<nostr::Filter>,
        progress_reporter: MultiProgress,
    ) -> Result<(Vec<Result<Vec<nostr::Event>>>, MultiProgress)> {
        // add relays
        for relay in &relays {
            self.client
                .add_relay(relay.as_str())
                .await
                .context("cannot add relay")?;
        }

        let relays_map = self.client.relays().await;

        let futures: Vec<_> = relays
            .clone()
            .iter()
            // don't look for events on blaster
            .filter(|r| !r.as_str().contains("nostr.mutinywallet.com"))
            .map(|r| (relays_map.get(r).unwrap(), filters.clone()))
            .map(|(relay, filters)| async {
                let pb = if std::env::var("NGITTEST").is_err() {
                    let pb = progress_reporter.add(
                        ProgressBar::new(1)
                            .with_prefix(format!("{: <11}{}", "connecting", relay.url()))
                            .with_style(pb_style()?),
                    );
                    pb.enable_steady_tick(Duration::from_millis(300));
                    Some(pb)
                } else {
                    None
                };
                #[allow(clippy::large_futures)]
                match get_events_of(relay, filters, &pb).await {
                    Err(error) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(false));
                            pb.set_prefix(format!("{: <11}{}", "error", relay.url()));
                            pb.finish_with_message(
                                console::style(
                                    error.to_string().replace("relay pool error:", "error:"),
                                )
                                .for_stderr()
                                .red()
                                .to_string(),
                            );
                        }
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
            })
            .collect();

        let relay_results: Vec<Result<Vec<nostr::Event>>> =
            stream::iter(futures).buffer_unordered(15).collect().await;

        Ok((relay_results, progress_reporter))
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_all(
        &self,
        git_repo_path: &Path,
        repo_coordinates: &HashSet<Coordinate>,
        user_profiles: &HashSet<PublicKey>,
    ) -> Result<(Vec<Result<FetchReport>>, MultiProgress)> {
        let fallback_relays = &self
            .fallback_relays
            .iter()
            .filter_map(|r| Url::parse(r).ok())
            .collect::<HashSet<Url>>();

        let mut request = create_relays_request(
            git_repo_path,
            repo_coordinates,
            user_profiles,
            fallback_relays.clone(),
        )
        .await?;

        let progress_reporter = MultiProgress::new();

        let mut processed_relays = HashSet::new();

        let mut relay_reports: Vec<Result<FetchReport>> = vec![];

        loop {
            let relays = request
                .repo_relays
                .union(&request.user_relays_for_profiles)
                // don't look for events on blaster
                .filter(|&r| !r.as_str().contains("nostr.mutinywallet.com"))
                .cloned()
                .collect::<HashSet<Url>>()
                .difference(&processed_relays)
                .cloned()
                .collect::<HashSet<Url>>();
            if relays.is_empty() {
                break;
            }
            let profile_relays_only = request
                .user_relays_for_profiles
                .difference(&request.repo_relays)
                .collect::<HashSet<&Url>>();
            for relay in &request.repo_relays {
                self.client
                    .add_relay(relay.as_str())
                    .await
                    .context("cannot add relay")?;
            }

            let dim = Style::new().color256(247);

            let futures: Vec<_> = relays
                .iter()
                .map(|r| {
                    if profile_relays_only.contains(r) {
                        // if relay isn't a repo relay, just filter for user profile
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
                .map(|request| async {
                    let relay_column_width = request.relay_column_width;

                    let relay_url = request
                        .selected_relay
                        .clone()
                        .context("fetch_all_from_relay called without a relay")?;

                    let pb = if std::env::var("NGITTEST").is_err() {
                        let pb = progress_reporter.add(
                            ProgressBar::new(1)
                                .with_prefix(
                                    dim.apply_to(format!(
                                        "{: <relay_column_width$} connecting",
                                        &relay_url
                                    ))
                                    .to_string(),
                                )
                                .with_style(pb_style()?),
                        );
                        pb.enable_steady_tick(Duration::from_millis(300));
                        Some(pb)
                    } else {
                        None
                    };

                    #[allow(clippy::large_futures)]
                    match self.fetch_all_from_relay(git_repo_path, request, &pb).await {
                        Err(error) => {
                            if let Some(pb) = pb {
                                pb.set_style(pb_after_style(false));
                                pb.set_prefix(
                                    dim.apply_to(format!("{: <relay_column_width$}", &relay_url))
                                        .to_string(),
                                );
                                pb.finish_with_message(
                                    console::style(
                                        error.to_string().replace("relay pool error:", "error:"),
                                    )
                                    .for_stderr()
                                    .red()
                                    .to_string(),
                                );
                            }
                            Err(error)
                        }
                        Ok(res) => Ok(res),
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

            if let Ok(repo_ref) = get_repo_ref_from_cache(git_repo_path, repo_coordinates).await {
                request.repo_relays = repo_ref
                    .relays
                    .iter()
                    .filter_map(|r| Url::parse(r).ok())
                    .collect();
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
                            if let Ok(url) = Url::parse(&r) {
                                set.insert(url);
                            }
                        }
                    }
                }
                set
            };
        }
        Ok((relay_reports, progress_reporter))
    }

    async fn fetch_all_from_relay(
        &self,
        git_repo_path: &Path,
        request: FetchRequest,
        pb: &Option<ProgressBar>,
    ) -> Result<FetchReport> {
        let mut fresh_coordinates: HashSet<Coordinate> = HashSet::new();
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

        self.connect(&relay_url).await?;

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
                    .to_string(),
                );
            }

            fresh_coordinates = HashSet::new();
            fresh_proposal_roots = HashSet::new();
            fresh_profiles = HashSet::new();

            let relay = self.client.relay(&relay_url).await?;
            let events: Vec<nostr::Event> = get_events_of(&relay, filters.clone(), &None)
                .await?
                .iter()
                // don't process events that don't match filters
                .filter(|e| filters.iter().any(|f| f.match_event(e)))
                .cloned()
                .collect();
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
            pb.set_prefix(
                dim.apply_to(format!(
                    "{: <relay_column_width$} {}",
                    relay_url,
                    if report.to_string().is_empty() {
                        "no new events".to_string()
                    } else {
                        format!("new events: {report}")
                    },
                ))
                .to_string(),
            );
            pb.finish_with_message("");
        }
        Ok(report)
    }
}

static CONNECTION_TIMEOUT: u64 = 3;
static GET_EVENTS_TIMEOUT: u64 = 7;

async fn get_events_of(
    relay: &nostr_sdk::Relay,
    filters: Vec<nostr::Filter>,
    pb: &Option<ProgressBar>,
) -> Result<Vec<Event>> {
    // relay.reconcile(filter, opts).await?;

    if !relay.is_connected().await {
        #[allow(clippy::large_futures)]
        relay
            .connect(Some(std::time::Duration::from_secs(CONNECTION_TIMEOUT)))
            .await;
    }

    if !relay.is_connected().await {
        bail!("connection timeout");
    } else if let Some(pb) = pb {
        pb.set_prefix(format!("connected  {}", relay.url()));
    }
    let events = relay
        .get_events_of(
            filters,
            // 20 is nostr_sdk default
            std::time::Duration::from_secs(GET_EVENTS_TIMEOUT),
            nostr_sdk::FilterOptions::ExitOnEOSE,
        )
        .await?;
    Ok(events)
}

#[derive(Default)]
pub struct Params {
    pub keys: Option<nostr::Keys>,
    pub fallback_relays: Vec<String>,
    pub more_fallback_relays: Vec<String>,
    pub blaster_relays: Vec<String>,
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

pub async fn sign_event(event_builder: EventBuilder, signer: &NostrSigner) -> Result<nostr::Event> {
    if signer.r#type().eq(&nostr_signer::NostrSignerType::NIP46) {
        let term = console::Term::stderr();
        term.write_line("signing event with remote signer...")?;
        let event = signer
            .sign_event_builder(event_builder)
            .await
            .context("failed to sign event")?;
        term.clear_last_lines(1)?;
        Ok(event)
    } else {
        signer
            .sign_event_builder(event_builder)
            .await
            .context("failed to sign event")
    }
}

pub async fn fetch_public_key(signer: &NostrSigner) -> Result<nostr::PublicKey> {
    let term = console::Term::stderr();
    term.write_line("fetching npub from remote signer...")?;
    let public_key = signer
        .public_key()
        .await
        .context("failed to get npub from remote signer")?;
    term.clear_last_lines(1)?;
    Ok(public_key)
}

fn pb_style() -> Result<ProgressStyle> {
    Ok(
        ProgressStyle::with_template(" {spinner} {prefix} {msg} {timeout_in}")?.with_key(
            "timeout_in",
            |state: &ProgressState, w: &mut dyn Write| {
                if state.elapsed().as_secs() > 3 && state.elapsed().as_secs() < GET_EVENTS_TIMEOUT {
                    let dim = Style::new().color256(247);
                    write!(
                        w,
                        "{}",
                        dim.apply_to(format!(
                            "timeout in {:.1}s",
                            GET_EVENTS_TIMEOUT - state.elapsed().as_secs()
                        ))
                    )
                    .unwrap();
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

async fn get_local_cache_database(git_repo_path: &Path) -> Result<SQLiteDatabase> {
    SQLiteDatabase::open(git_repo_path.join(".git/nostr-cache.sqlite"))
        .await
        .context("cannot open or create nostr cache database at .git/nostr-cache.sqlite")
}

async fn get_global_cache_database(git_repo_path: &Path) -> Result<SQLiteDatabase> {
    SQLiteDatabase::open(if std::env::var("NGITTEST").is_err() {
        create_dir_all(get_dirs()?.cache_dir()).context(format!(
            "cannot create cache directory in: {:?}",
            get_dirs()?.cache_dir()
        ))?;
        get_dirs()?.cache_dir().join("nostr-cache.sqlite")
    } else {
        git_repo_path.join(".git/test-global-cache.sqlite")
    })
    .await
    .context("cannot open ngit global nostr cache database")
}

pub async fn get_events_from_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
    get_local_cache_database(git_repo_path)
        .await?
        .query(filters.clone(), Order::Asc)
        .await
        .context(
            "cannot execute query on opened git repo nostr cache database .git/nostr-cache.sqlite",
        )
}

pub async fn get_event_from_global_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
    get_global_cache_database(git_repo_path)
        .await?
        .query(filters.clone(), Order::Asc)
        .await
        .context("cannot execute query on opened ngit nostr cache database")
}

pub async fn save_event_in_cache(git_repo_path: &Path, event: &nostr::Event) -> Result<bool> {
    get_local_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("cannot save event in local cache")
}

pub async fn save_event_in_global_cache(
    git_repo_path: &Path,
    event: &nostr::Event,
) -> Result<bool> {
    get_global_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("cannot save event in local cache")
}

pub async fn get_repo_ref_from_cache(
    git_repo_path: &Path,
    repo_coordinates: &HashSet<Coordinate>,
) -> Result<RepoRef> {
    let mut maintainers = HashSet::new();
    let mut new_coordinate: bool;

    for c in repo_coordinates {
        maintainers.insert(c.public_key);
    }
    let mut repo_events = vec![];
    loop {
        new_coordinate = false;
        let repo_events_filter = get_filter_repo_events(repo_coordinates);

        let events = [
            get_event_from_global_cache(git_repo_path, vec![repo_events_filter.clone()]).await?,
            get_events_from_cache(git_repo_path, vec![repo_events_filter]).await?,
        ]
        .concat();
        for e in events {
            if let Ok(repo_ref) = RepoRef::try_from(e.clone()) {
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
    let repo_ref = RepoRef::try_from(
        repo_events
            .first()
            .context("no repo events at specified coordinates")?
            .clone(),
    )?;

    let mut events: HashMap<Coordinate, nostr::Event> = HashMap::new();
    for m in &maintainers {
        if let Some(e) = repo_events.iter().find(|e| e.author().eq(m)) {
            events.insert(
                Coordinate {
                    kind: e.kind,
                    identifier: e.identifier().unwrap().to_string(),
                    public_key: e.author(),
                    relays: vec![],
                },
                e.clone(),
            );
        }
    }

    Ok(RepoRef {
        // use all maintainers from all events found, not just maintainers in the most
        // recent event
        maintainers: maintainers.iter().copied().collect::<Vec<PublicKey>>(),
        events,
        ..repo_ref
    })
}

pub async fn get_state_from_cache(git_repo_path: &Path, repo_ref: &RepoRef) -> Result<RepoState> {
    RepoState::try_from(
        get_events_from_cache(
            git_repo_path,
            vec![get_filter_state_events(&repo_ref.coordinates())],
        )
        .await?,
    )
}

#[allow(clippy::too_many_lines)]
async fn create_relays_request(
    git_repo_path: &Path,
    repo_coordinates: &HashSet<Coordinate>,
    user_profiles: &HashSet<PublicKey>,
    fallback_relays: HashSet<Url>,
) -> Result<FetchRequest> {
    let repo_ref = get_repo_ref_from_cache(git_repo_path, repo_coordinates).await;

    let repo_coordinates = {
        // add coordinates of users listed in maintainers to explicitly specified
        // coodinates
        let mut repo_coordinates = repo_coordinates.clone();
        if let Ok(repo_ref) = &repo_ref {
            for c in repo_ref.coordinates() {
                if !repo_coordinates
                    .iter()
                    .any(|e| e.identifier.eq(&c.identifier) && e.public_key.eq(&c.public_key))
                {
                    repo_coordinates.insert(c);
                }
            }
        }
        repo_coordinates
    };

    let repo_coordinates_without_relays = {
        let mut set = HashSet::new();
        for c in &repo_coordinates {
            set.insert(Coordinate {
                kind: c.kind,
                identifier: c.identifier.clone(),
                public_key: c.public_key,
                relays: vec![],
            });
        }
        set
    };

    let mut proposals: HashSet<EventId> = HashSet::new();
    let mut missing_contributor_profiles: HashSet<PublicKey> = HashSet::new();
    let mut contributors: HashSet<PublicKey> = HashSet::new();

    if !repo_coordinates_without_relays.is_empty() {
        if let Ok(repo_ref) = &repo_ref {
            for m in &repo_ref.maintainers {
                contributors.insert(m.to_owned());
            }
        }

        for event in &get_events_from_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(vec![Kind::GitPatch])
                    .custom_tag(
                        SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                        repo_coordinates_without_relays
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<String>>(),
                    ),
            ],
        )
        .await?
        {
            if event_is_patch_set_root(event) || event_is_revision_root(event) {
                proposals.insert(event.id());
                contributors.insert(event.author());
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
                .find(|e| e.kind() == Kind::Metadata && e.author().eq(c))
            {
                save_event_in_cache(git_repo_path, event).await?;
            } else {
                missing_contributor_profiles.insert(c.to_owned());
            }
        }
    }

    let profiles_to_fetch_from_user_relays = {
        let mut user_profiles = user_profiles.clone();
        if let Ok(Some(current_user)) = get_logged_in_user(git_repo_path).await {
            user_profiles.insert(current_user);
        }
        let mut map: HashMap<PublicKey, (Timestamp, Timestamp)> = HashMap::new();
        for public_key in &user_profiles {
            if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
                map.insert(
                    public_key.to_owned(),
                    (user_ref.metadata.created_at, user_ref.relays.created_at),
                );
            } else {
                map.insert(
                    public_key.to_owned(),
                    (Timestamp::from(0), Timestamp::from(0)),
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
                    if let Ok(url) = Url::parse(&r) {
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
            for (id, _) in get_local_cache_database(git_repo_path)
                .await?
                .negentropy_items(filter)
                .await?
            {
                existing_events.insert(id);
            }
        }
        existing_events
    };

    let relays = {
        let mut relays = fallback_relays;
        if let Ok(repo_ref) = &repo_ref {
            for r in &repo_ref.relays {
                if let Ok(url) = Url::parse(r) {
                    relays.insert(url);
                }
            }
        }
        for c in repo_coordinates {
            for r in &c.relays {
                if let Ok(url) = Url::parse(r) {
                    relays.insert(url);
                }
            }
        }
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
        repo_coordinates_without_relays: if let Ok(repo_ref) = &repo_ref {
            repo_ref.coordinates_with_timestamps()
        } else {
            repo_coordinates_without_relays
                .iter()
                .map(|c| (c.clone(), None))
                .collect()
        },
        state: if let Ok(repo_ref) = &repo_ref {
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
    git_repo_path: &Path,
    fresh_coordinates: &mut HashSet<Coordinate>,
    fresh_proposal_roots: &mut HashSet<EventId>,
    fresh_profiles: &mut HashSet<PublicKey>,
    report: &mut FetchReport,
) -> Result<()> {
    for event in &events {
        if !request.existing_events.contains(&event.id) {
            save_event_in_cache(git_repo_path, event).await?;
            if event.kind().eq(&Kind::GitRepoAnnouncement) {
                save_event_in_global_cache(git_repo_path, event).await?;
                let new_coordinate = !request
                    .repo_coordinates_without_relays
                    .iter()
                    .map(|(c, _)| c.clone())
                    .any(|c| {
                        c.identifier.eq(event.identifier().unwrap())
                            && c.public_key.eq(&event.pubkey)
                    });
                let update_to_existing = !new_coordinate
                    && request
                        .repo_coordinates_without_relays
                        .iter()
                        .any(|(c, t)| {
                            c.identifier.eq(event.identifier().unwrap())
                                && c.public_key.eq(&event.pubkey)
                                && if let Some(t) = t {
                                    event.created_at.gt(t)
                                } else {
                                    true
                                }
                        });
                if update_to_existing {
                    report.updated_repo_announcements.push((
                        Coordinate {
                            kind: event.kind(),
                            public_key: event.author(),
                            identifier: event.identifier().unwrap().to_owned(),
                            relays: vec![],
                        },
                        event.created_at,
                    ));
                }
                // if contains new maintainer
                if let Ok(repo_ref) = &RepoRef::try_from(event.clone()) {
                    for m in &repo_ref.maintainers {
                        if !request
                            .repo_coordinates_without_relays // prexisting maintainers
                            .iter()
                            .map(|(c, _)| c.clone())
                            .collect::<HashSet<Coordinate>>()
                            .union(&report.repo_coordinates_without_relays) // already added maintainers
                            .any(|c| c.identifier.eq(&repo_ref.identifier) && m.eq(&c.public_key))
                        {
                            let c = Coordinate {
                                kind: event.kind(),
                                public_key: *m,
                                identifier: repo_ref.identifier.clone(),
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
            } else if event.kind().eq(&STATE_KIND) {
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
            } else if event_is_patch_set_root(event) {
                fresh_proposal_roots.insert(event.id);
                report.proposals.insert(event.id);
                if !request.contributors.contains(&event.author())
                    && !fresh_profiles.contains(&event.author())
                {
                    fresh_profiles.insert(event.author());
                }
            } else if [Kind::RelayList, Kind::Metadata].contains(&event.kind()) {
                if request
                    .missing_contributor_profiles
                    .contains(&event.author())
                {
                    report.contributor_profiles.insert(event.author());
                } else if let Some((_, (metadata_timestamp, relay_list_timestamp))) = request
                    .profiles_to_fetch_from_user_relays
                    .get_key_value(&event.author())
                {
                    if (Kind::Metadata.eq(&event.kind())
                        && event.created_at().gt(metadata_timestamp))
                        || (Kind::RelayList.eq(&event.kind())
                            && event.created_at().gt(relay_list_timestamp))
                    {
                        report.profile_updates.insert(event.author());
                    }
                }
                save_event_in_global_cache(git_repo_path, event).await?;
            }
        }
    }
    for event in &events {
        if !request.existing_events.contains(&event.id)
            && !event.event_ids().any(|id| report.proposals.contains(id))
        {
            if event.kind().eq(&Kind::GitPatch) && !event_is_patch_set_root(event) {
                report.commits.insert(event.id);
            } else if status_kinds().contains(&event.kind()) {
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
    repo_coordinates: &HashSet<Coordinate>,
    proposal_ids: &HashSet<EventId>,
    required_profiles: &HashSet<PublicKey>,
) -> Vec<nostr::Filter> {
    [
        if repo_coordinates.is_empty() {
            vec![]
        } else {
            vec![
                get_filter_state_events(repo_coordinates),
                get_filter_repo_events(repo_coordinates),
                nostr::Filter::default()
                    .kinds(vec![Kind::GitPatch, Kind::EventDeletion])
                    .custom_tag(
                        SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                        repo_coordinates
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<String>>(),
                    ),
            ]
        },
        if proposal_ids.is_empty() {
            vec![]
        } else {
            vec![
                nostr::Filter::default()
                    .events(proposal_ids.clone())
                    .kinds([vec![Kind::GitPatch, Kind::EventDeletion], status_kinds()].concat()),
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

pub fn get_filter_repo_events(repo_coordinates: &HashSet<Coordinate>) -> nostr::Filter {
    nostr::Filter::default()
        .kind(Kind::GitRepoAnnouncement)
        .identifiers(
            repo_coordinates
                .iter()
                .map(|c| c.identifier.clone())
                .collect::<Vec<String>>(),
        )
        .authors(
            repo_coordinates
                .iter()
                .map(|c| c.public_key)
                .collect::<Vec<PublicKey>>(),
        )
}

pub static STATE_KIND: nostr::Kind = Kind::Custom(30618);
pub fn get_filter_state_events(repo_coordinates: &HashSet<Coordinate>) -> nostr::Filter {
    nostr::Filter::default()
        .kind(STATE_KIND)
        .identifiers(
            repo_coordinates
                .iter()
                .map(|c| c.identifier.clone())
                .collect::<Vec<String>>(),
        )
        .authors(
            repo_coordinates
                .iter()
                .map(|c| c.public_key)
                .collect::<Vec<PublicKey>>(),
        )
}

pub fn get_filter_contributor_profiles(contributors: HashSet<PublicKey>) -> nostr::Filter {
    nostr::Filter::default()
        .kinds(vec![Kind::Metadata, Kind::RelayList])
        .authors(contributors)
}

#[derive(Default)]
pub struct FetchReport {
    repo_coordinates_without_relays: HashSet<Coordinate>,
    updated_repo_announcements: Vec<(Coordinate, Timestamp)>,
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
        // report: "1 new maintainer, 1 announcement, 1 proposal, 3 commits, 2 statuses"
        let mut display_items: Vec<String> = vec![];
        if !self.repo_coordinates_without_relays.is_empty() {
            display_items.push(format!(
                "{} new maintainer{}",
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
    repo_relays: HashSet<Url>,
    selected_relay: Option<Url>,
    relay_column_width: usize,
    repo_coordinates_without_relays: Vec<(Coordinate, Option<Timestamp>)>,
    state: Option<(Timestamp, EventId)>,
    proposals: HashSet<EventId>,
    contributors: HashSet<PublicKey>,
    missing_contributor_profiles: HashSet<PublicKey>,
    existing_events: HashSet<EventId>,
    profiles_to_fetch_from_user_relays: HashMap<PublicKey, (Timestamp, Timestamp)>,
    user_relays_for_profiles: HashSet<Url>,
}

pub async fn fetching_with_report(
    git_repo_path: &Path,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_coordinates: &HashSet<Coordinate>,
) -> Result<FetchReport> {
    let term = console::Term::stderr();
    term.write_line("fetching updates...")?;
    let (relay_reports, progress_reporter) = client
        .fetch_all(git_repo_path, repo_coordinates, &HashSet::new())
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
    repo_coordinates: HashSet<Coordinate>,
) -> Result<Vec<nostr::Event>> {
    let mut proposals = get_events_from_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .kind(nostr::Kind::GitPatch)
                .custom_tag(
                    nostr::SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                    repo_coordinates
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<String>>(),
                ),
        ],
    )
    .await?
    .iter()
    .filter(|e| event_is_patch_set_root(e))
    .cloned()
    .collect::<Vec<nostr::Event>>();
    proposals.sort_by_key(|e| e.created_at);
    proposals.reverse();
    Ok(proposals)
}

pub async fn get_all_proposal_patch_events_from_cache(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    proposal_id: &nostr::EventId,
) -> Result<Vec<nostr::Event>> {
    let mut commit_events = get_events_from_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .kind(nostr::Kind::GitPatch)
                .event(*proposal_id),
            nostr::Filter::default()
                .kind(nostr::Kind::GitPatch)
                .id(*proposal_id),
        ],
    )
    .await?;

    let permissioned_users: HashSet<PublicKey> = [
        repo_ref.maintainers.clone(),
        vec![
            commit_events
                .iter()
                .find(|e| e.id().eq(proposal_id))
                .context("proposal not in cache")?
                .author(),
        ],
    ]
    .concat()
    .iter()
    .copied()
    .collect();
    commit_events.retain(|e| permissioned_users.contains(&e.author()));

    let revision_roots: HashSet<nostr::EventId> = commit_events
        .iter()
        .filter(|e| event_is_revision_root(e))
        .map(nostr::Event::id)
        .collect();

    if !revision_roots.is_empty() {
        for event in get_events_from_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::GitPatch)
                    .events(revision_roots)
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
        .filter(|e| !event_is_cover_letter(e) && permissioned_users.contains(&e.author()))
        .cloned()
        .collect())
}

pub async fn get_event_from_cache_by_id(git_repo: &Repo, event_id: &EventId) -> Result<Event> {
    Ok(get_events_from_cache(
        git_repo.get_path()?,
        vec![nostr::Filter::default().id(*event_id)],
    )
    .await?
    .first()
    .context("cannot find event in cache")?
    .clone())
}

#[allow(clippy::module_name_repetitions)]
#[allow(clippy::too_many_lines)]
pub async fn send_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    git_repo_path: &Path,
    events: Vec<nostr::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<String>,
    animate: bool,
    silent: bool,
) -> Result<()> {
    let fallback = [
        client.get_fallback_relays().clone(),
        if events
            .iter()
            .any(|e| e.kind().eq(&Kind::GitRepoAnnouncement))
        {
            client.get_blaster_relays().clone()
        } else {
            vec![]
        },
    ]
    .concat();
    let mut relays: Vec<&String> = vec![];

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

    let m = if silent {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
    } else {
        MultiProgress::new()
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
    join_all(relays.iter().map(|&relay| async {
        let relay_clean = remove_trailing_slash(&*relay);
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
                .any(|r| relay_clean.eq(&remove_trailing_slash(r)))
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
                .send_event_to(git_repo_path, relay.as_str(), event.clone())
                .await
            {
                Ok(_) => pb.inc(1),
                Err(e) => {
                    pb.set_style(pb_after_style_failed.clone());
                    pb.finish_with_message(
                        console::style(
                            e.to_string()
                                .replace("relay pool error:", "error:")
                                .replace("event not published: ", "error: "),
                        )
                        .for_stderr()
                        .red()
                        .to_string(),
                    );
                    failed = true;
                    break;
                }
            };
        }
        if !failed {
            pb.set_style(pb_after_style_succeeded.clone());
            pb.finish_with_message("");
        }
    }))
    .await;
    Ok(())
}

fn remove_trailing_slash(s: &String) -> String {
    match s.as_str().strip_suffix('/') {
        Some(s) => s,
        None => s,
    }
    .to_string()
}
