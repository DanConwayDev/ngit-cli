//! Auto-accept co-maintainership when publishing maintainer events.
//!
//! When a user has been offered co-maintainership (they appear in another
//! maintainer's `maintainers` tag but have never published their own
//! Kind:30617 announcement), pushing would normally fail. This module
//! provides helpers to publish the co-maintainer's announcement with sensible
//! defaults before, or batched with, the maintainer's own event.
//!
//! See `docs/design/co-maintainer-announcement-rationale.md` for why the
//! announcement is required (scam-protection) even though the fetch/read side
//! already trusts state events from all listed maintainers.
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use futures::future::join_all;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use nostr::prelude::{Event, PublicKey, RelayUrl, Timestamp};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::{Connect, send_events},
    git::{Repo, RepoActions},
    git_http_auth::refresh_private_git_auth_for_url,
    login::user::{UserRef, publish_private_git_relay_list},
    repo_ref::{
        RepoRef, apply_grasp_infrastructure, format_grasp_server_url_as_clone_url,
        latest_event_repo_ref,
    },
    signer::NgitSigner,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// A prepared co-maintainer announcement and the metadata needed to finalize
/// local repository state after it has been published.
pub struct MaintainerAcceptance {
    pub event: Event,
    pub relays: Vec<RelayUrl>,
    selected_grasp_servers: Vec<String>,
    public_key: PublicKey,
    identifier: String,
    private_signer: Option<Arc<NgitSigner>>,
}

/// Lead relationship an acceptance may safely acknowledge.
///
/// A resolved lead is preferred. A pending direct `M` may be completed only
/// by that named pubkey accepting; an ordinary invitee must not confirm an
/// unaccepted third-party lead.
pub fn acceptance_lead(repo_ref: &RepoRef, my_pubkey: PublicKey) -> Option<PublicKey> {
    repo_ref
        .lead_maintainer()
        .or_else(|| repo_ref.lead.filter(|lead| *lead == my_pubkey))
}

/// Maintainers to list when accepting without an explicit relationship choice.
///
/// Follows NIP-34's SHOULD: when `M` is used, `m` and `o` authors list only
/// themselves and the lead — so the lead can change or remove them
/// unilaterally — while an accepting author who is themselves the lead keeps
/// the full listing, since the lead's announcement defines the membership.
/// A wire-asserted lead is only reciprocated when already confirmed: listing
/// an unconfirmed lead alone could not make the accepter's own announcement
/// acknowledge a confirmed member. Without a usable lead, reciprocate the
/// sole confirmed maintainer, retaining the selected maintainer for
/// backwards-compatible, non-interactive operation on ambiguous graphs.
pub fn default_acceptance_maintainers(repo_ref: &RepoRef, my_pubkey: PublicKey) -> Vec<PublicKey> {
    let lead = acceptance_lead(repo_ref, my_pubkey);
    if lead == Some(my_pubkey) {
        let mut maintainers = vec![my_pubkey];
        for maintainer in &repo_ref.maintainers {
            if !maintainers.contains(maintainer) {
                maintainers.push(*maintainer);
            }
        }
        return maintainers;
    }

    let confirmed = repo_ref.confirmed_maintainers();
    let preferred = lead
        .filter(|lead| confirmed.contains(lead))
        .or_else(|| {
            if confirmed.len() == 1 {
                confirmed.first().copied()
            } else {
                None
            }
        })
        .unwrap_or(repo_ref.selected_maintainer);

    let mut maintainers = vec![my_pubkey];
    if preferred != my_pubkey {
        maintainers.push(preferred);
    }
    maintainers
}

/// Build the co-maintainer's own Kind:30617 announcement with defaults.
///
/// The caller is responsible for publishing `MaintainerAcceptance::event` to
/// `MaintainerAcceptance::relays`, optionally batched with another event, and
/// then calling `finalize_maintainership_acceptance`.
pub async fn build_maintainership_acceptance_with_defaults(
    repo_ref: &RepoRef,
    user_ref: &UserRef,
    #[cfg(test)] client: &MockConnect,
    #[cfg(not(test))] client: &Client,
    signer: &Arc<crate::signer::NgitSigner>,
) -> Result<MaintainerAcceptance> {
    let my_pubkey = &user_ref.public_key;
    let identifier = &repo_ref.identifier;

    // --- Step 1: resolve infrastructure ---

    let selected_grasp_servers =
        grasp_servers_from_user_or_fallback(user_ref, Some(repo_ref), client);

    let mut git_servers: Vec<String> = vec![];
    let mut relay_strings: Vec<String> = vec![];

    apply_grasp_infrastructure(
        &selected_grasp_servers,
        &mut git_servers,
        &mut relay_strings,
        my_pubkey,
        identifier,
    )?;

    let relays: Vec<RelayUrl> = relay_strings
        .iter()
        .filter_map(|r| RelayUrl::parse(r).ok())
        .collect();

    // --- Step 2: resolve shared metadata from latest existing event ---

    let latest = latest_event_repo_ref(repo_ref);
    let name = latest
        .as_ref()
        .map(|lr| lr.name.clone())
        .unwrap_or_else(|| identifier.clone());
    let description = latest
        .as_ref()
        .map(|lr| lr.description.clone())
        .unwrap_or_default();
    let web = latest.as_ref().map(|lr| lr.web.clone()).unwrap_or_default();
    let upstream = latest
        .as_ref()
        .map(|lr| lr.upstream.clone())
        .unwrap_or_default();
    let hashtags = latest
        .as_ref()
        .map(|lr| lr.hashtags.clone())
        .unwrap_or_default();
    let blossoms = latest
        .as_ref()
        .map(|lr| lr.blossoms.clone())
        .unwrap_or_default();
    let root_commit = latest
        .as_ref()
        .map(|lr| lr.root_commit.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| repo_ref.root_commit.clone());

    // --- Step 3: reciprocate the lead or sole maintainer ---

    let maintainers = default_acceptance_maintainers(repo_ref, *my_pubkey);
    // per NIP-34 the acceptance re-asserts the repository's wire lead as
    // `M`; the guard is defensive — a lead reported by lead_maintainer()
    // always ends up in the default listing
    let lead = acceptance_lead(repo_ref, *my_pubkey).filter(|lead| maintainers.contains(lead));

    // --- Step 4: build RepoRef ---

    let role_tags = repo_ref.role_history_for_acceptance(
        my_pubkey,
        &maintainers,
        lead,
        Timestamp::now().as_secs(),
    );
    let my_repo_ref = RepoRef {
        identifier: identifier.clone(),
        name: name.clone(),
        description,
        root_commit,
        git_server: git_servers,
        web,
        upstream,
        relays: relays.clone(),
        blossoms,
        hashtags,
        private: repo_ref.private,
        selected_maintainer: *my_pubkey,
        maintainers_without_annoucnement: None,
        maintainers,
        events: HashMap::new(),
        nostr_git_url: None,
        extra_tags: vec![],
        role_tags,
        moderators: vec![],
        lead,
    };

    // --- Step 5: sign the announcement ---

    eprintln!(
        "info: accepting co-maintainership of '{}' with defaults",
        name
    );

    let event = my_repo_ref.to_event(signer).await?;

    Ok(MaintainerAcceptance {
        event,
        relays,
        selected_grasp_servers,
        public_key: *my_pubkey,
        identifier: identifier.clone(),
        private_signer: repo_ref.private.then(|| signer.clone()),
    })
}

/// Wait for grasp servers to provision the bare repository after a prepared
/// co-maintainer announcement has been published.
pub async fn finalize_maintainership_acceptance(
    git_repo: &Repo,
    acceptance: &MaintainerAcceptance,
) -> Result<()> {
    if !acceptance.selected_grasp_servers.is_empty() {
        wait_for_grasp_servers(
            git_repo,
            &acceptance.selected_grasp_servers,
            &acceptance.public_key,
            &acceptance.identifier,
            acceptance.private_signer.clone(),
        )
        .await?;
    }

    // Deliberately leave `nostr.repo` and the origin remote untouched: the
    // coordinate the repo resolves from is the root of trust. Re-rooting
    // resolution on the accepter's own announcement — which always lists
    // them as a maintainer — would make it impossible to observe the
    // inviter removing them later. Keeping resolution on the inviter's
    // coordinate means removal surfaces naturally; only `ngit repo edit` /
    // `ngit init` may change the resolved coordinate deliberately.

    eprintln!("info: co-maintainership accepted. run `ngit init` to customise your announcement.");

    Ok(())
}

/// Publish the co-maintainer's own Kind:30617 announcement with defaults.
///
/// The local `nostr.repo` config and remotes are left untouched so the
/// repository keeps resolving from the inviter's coordinate (see
/// `finalize_maintainership_acceptance`).
///
/// This is called automatically from the push path when the pushing user is
/// listed as a maintainer but has not yet published their own announcement.
/// No interactive prompts are shown — all values come from the existing
/// announcement and the user's saved grasp server / relay preferences.
pub async fn accept_maintainership_with_defaults(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    user_ref: &UserRef,
    #[cfg(test)] client: &mut MockConnect,
    #[cfg(not(test))] client: &mut Client,
    signer: &Arc<crate::signer::NgitSigner>,
) -> Result<()> {
    let acceptance =
        build_maintainership_acceptance_with_defaults(repo_ref, user_ref, client, signer).await?;
    eprintln!("info: publishing your repository announcement to nostr...");

    client.set_signer(signer.clone()).await;

    if repo_ref.private {
        publish_private_git_relay_list(client, &acceptance.relays, user_ref, signer)
            .await
            .context("failed to publish private Git relay discovery list")?;
    }

    let _ = send_events(
        client,
        Some(git_repo.get_path()?),
        vec![acceptance.event.clone()],
        user_ref.relays.write(),
        acceptance.relays.clone(),
        false, // no spinner — we are mid-push
        true,  // silent
    )
    .await
    .context("failed to publish co-maintainer announcement")?;

    finalize_maintainership_acceptance(git_repo, &acceptance).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Grasp server helpers
// ---------------------------------------------------------------------------

/// Return grasp servers for a co-maintainer using the following priority:
///
/// 1. User's own saved grasp server list (if non-empty).
/// 2. Selected maintainer's grasp servers derived from
///    `selected_maintainer_repo_ref` (if provided and non-empty). If the
///    selected maintainer only uses a single grasp server, the first
///    system-default grasp server is appended so the co-maintainer has at least
///    two servers for redundancy.
/// 3. System / client default grasp servers.
pub fn grasp_servers_from_user_or_fallback(
    user_ref: &UserRef,
    selected_maintainer_repo_ref: Option<&RepoRef>,
    #[cfg(test)] client: &MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Vec<String> {
    // Priority 1: user's own grasp list.
    if !user_ref.grasp_list.urls.is_empty() {
        return user_ref
            .grasp_list
            .urls
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
    }

    // Priority 2: selected maintainer's grasp servers.
    if let Some(rr) = selected_maintainer_repo_ref {
        let maintainer_servers = rr.grasp_servers();
        if !maintainer_servers.is_empty() {
            if maintainer_servers.len() == 1 {
                // Supplement a single server with the first system default for
                // redundancy, avoiding duplicates.
                let mut servers = maintainer_servers;
                if let Some(first_default) = client.get_grasp_default_set().first() {
                    if !servers.contains(first_default) {
                        servers.push(first_default.clone());
                    }
                }
                return servers;
            }
            return maintainer_servers;
        }
    }

    // Priority 3: system defaults.
    client
        .get_grasp_default_set()
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Grasp server provisioning poll
// ---------------------------------------------------------------------------

/// Holds the final style + message for a bar that completed before the detail
/// view was revealed.
struct DeferredServerFinish {
    bar: ProgressBar,
    style: ProgressStyle,
    message: String,
}

struct ServerRevealState {
    revealed: AtomicBool,
    deferred: Mutex<Vec<DeferredServerFinish>>,
}

struct PollContext {
    timeout_secs: u64,
    total: u64,
    ready_count: Arc<AtomicU64>,
    spinner_pb: ProgressBar,
    reveal_state: Arc<ServerRevealState>,
    private_signer: Option<Arc<NgitSigner>>,
}

fn check_git_server_ready(git_repo_path: &std::path::Path, git_server_url: &str) -> bool {
    let Ok(git_repo) = git2::Repository::open(git_repo_path) else {
        return false;
    };
    let Ok(mut remote) = git_repo.remote_anonymous(git_server_url) else {
        return false;
    };
    let mut fetch_options = git2::FetchOptions::new();
    let authorization = crate::git_http_auth::authorization_for_url(git_server_url);
    if let Some(header) = authorization.as_deref() {
        fetch_options.custom_headers(&[header]);
    }
    match remote.download(&[] as &[&str], Some(&mut fetch_options)) {
        Ok(()) => {
            let _ = remote.disconnect();
            true
        }
        Err(_) => false,
    }
}

fn create_server_bars(clone_urls: &[String], detail_multi: &MultiProgress) -> Vec<ProgressBar> {
    let waiting_style = ProgressStyle::with_template("  {spinner} {msg}")
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈");
    clone_urls
        .iter()
        .map(|url| {
            let name = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string();
            detail_multi.add(
                ProgressBar::new_spinner()
                    .with_style(waiting_style.clone())
                    .with_message(
                        console::style(format!("{name} - waiting"))
                            .for_stderr()
                            .dim()
                            .to_string(),
                    ),
            )
        })
        .collect()
}

fn spawn_expand_timer(
    expand_delay_ms: u64,
    spinner_pb: ProgressBar,
    detail_multi: MultiProgress,
    heading_bar: ProgressBar,
    reveal_state: Arc<ServerRevealState>,
    server_bars: Vec<ProgressBar>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(expand_delay_ms)).await;
        spinner_pb.finish_and_clear();
        detail_multi.set_draw_target(ProgressDrawTarget::stderr());
        heading_bar.finish_with_message("waiting for servers to create bare git repo...");
        let mut deferred = reveal_state.deferred.lock().unwrap();
        reveal_state.revealed.store(true, Ordering::Release);
        for df in deferred.drain(..) {
            df.bar.set_style(df.style);
            df.bar.finish_with_message(df.message);
        }
        for bar in &server_bars {
            if !bar.is_finished() {
                bar.enable_steady_tick(Duration::from_millis(100));
            }
        }
    })
}

fn finalize_spinner(all_ready: bool, spinner_pb: &ProgressBar, final_ready: u64, total: u64) {
    if all_ready {
        spinner_pb.finish_and_clear();
    } else {
        spinner_pb.set_style(ProgressStyle::with_template("{msg}").unwrap());
        spinner_pb.finish_with_message(format!(
            "timed out waiting for servers to create bare git repo ({final_ready}/{total} - complete), proceeding anyway"
        ));
    }
}

fn finish_server_bar(
    bar: &ProgressBar,
    style: ProgressStyle,
    message: String,
    reveal_state: &Arc<ServerRevealState>,
) {
    let mut deferred = reveal_state.deferred.lock().unwrap();
    if reveal_state.revealed.load(Ordering::Acquire) {
        drop(deferred);
        bar.set_style(style);
        bar.finish_with_message(message);
    } else {
        bar.set_style(style.clone());
        deferred.push(DeferredServerFinish {
            bar: bar.clone(),
            style,
            message,
        });
    }
}

async fn refresh_poll_authorization(url: &str, private_signer: Option<&Arc<NgitSigner>>) -> bool {
    match private_signer {
        Some(signer) => refresh_private_git_auth_for_url(url, signer).await.is_ok(),
        None => true,
    }
}

async fn poll_single_server(
    url: String,
    git_repo_path: std::path::PathBuf,
    bar: ProgressBar,
    ctx: Arc<PollContext>,
) -> bool {
    let poll_interval = Duration::from_millis(500);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(ctx.timeout_secs);
    let mut ready = false;
    loop {
        if !refresh_poll_authorization(&url, ctx.private_signer.as_ref()).await {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(poll_interval).await;
            continue;
        }

        let is_ready = tokio::task::spawn_blocking({
            let url = url.clone();
            let path = git_repo_path.clone();
            move || check_git_server_ready(&path, &url)
        })
        .await
        .unwrap_or(false);

        if is_ready {
            ready = true;
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }

        tokio::time::sleep(poll_interval).await;
    }

    let count = if ready {
        ctx.ready_count.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        ctx.ready_count.load(Ordering::Relaxed)
    };

    ctx.spinner_pb.set_message(format!(
        "waiting for servers to create bare git repo... ({count}/{total} - complete)",
        total = ctx.total
    ));

    let name = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    if ready {
        let style = ProgressStyle::with_template(&format!(
            "  {} {{msg}}",
            console::style("✔").for_stderr().green()
        ))
        .unwrap();
        let msg = console::style(format!("{name} - ready"))
            .for_stderr()
            .green()
            .to_string();
        finish_server_bar(&bar, style, msg, &ctx.reveal_state);
    } else {
        let style = ProgressStyle::with_template(&format!(
            "  {} {{msg}}",
            console::style("✘").for_stderr().red()
        ))
        .unwrap();
        let msg = console::style(format!("{name} - timeout"))
            .for_stderr()
            .red()
            .to_string();
        finish_server_bar(&bar, style, msg, &ctx.reveal_state);
    }

    ready
}

/// Poll grasp servers in parallel until all are ready or timeout is reached.
///
/// Shows a concise spinner with `x/y - complete` progress. After 5 s without
/// all servers responding, expands to show per-server status bars (including
/// any that already finished). Times out after 15 s (2 s in tests) and
/// proceeds rather than failing.
pub async fn wait_for_grasp_servers(
    git_repo: &Repo,
    grasp_servers: &[String],
    public_key: &PublicKey,
    identifier: &str,
    private_signer: Option<Arc<NgitSigner>>,
) -> Result<()> {
    let clone_urls: Vec<String> = grasp_servers
        .iter()
        .filter_map(|gs| format_grasp_server_url_as_clone_url(gs, public_key, identifier).ok())
        .collect();

    if clone_urls.is_empty() {
        return Ok(());
    }

    let is_test = std::env::var("NGITTEST").is_ok();
    let timeout_secs: u64 = if is_test { 2 } else { 15 };
    let expand_delay_ms: u64 = if is_test { 500 } else { 5000 };
    let total = clone_urls.len() as u64;

    let spinner_multi = MultiProgress::new();
    let spinner_pb = spinner_multi.add(
        ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .unwrap()
                    .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈"),
            )
            .with_message(format!(
                "waiting for servers to create bare git repo... (0/{total} - complete)"
            )),
    );
    spinner_pb.enable_steady_tick(Duration::from_millis(100));

    let detail_multi = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
    let heading_bar = detail_multi
        .add(ProgressBar::new(0).with_style(ProgressStyle::with_template("{msg}").unwrap()));

    let ready_count = Arc::new(AtomicU64::new(0));
    let reveal_state = Arc::new(ServerRevealState {
        revealed: AtomicBool::new(false),
        deferred: Mutex::new(Vec::new()),
    });

    let server_bars = create_server_bars(&clone_urls, &detail_multi);

    let timer_handle = spawn_expand_timer(
        expand_delay_ms,
        spinner_pb.clone(),
        detail_multi.clone(),
        heading_bar,
        reveal_state.clone(),
        server_bars.clone(),
    );

    let git_repo_path = git_repo.get_path()?.to_path_buf();
    let poll_ctx = Arc::new(PollContext {
        timeout_secs,
        total,
        ready_count: ready_count.clone(),
        spinner_pb: spinner_pb.clone(),
        reveal_state: reveal_state.clone(),
        private_signer,
    });
    let futures: Vec<_> = clone_urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            poll_single_server(
                url.clone(),
                git_repo_path.clone(),
                server_bars[i].clone(),
                poll_ctx.clone(),
            )
        })
        .collect();

    let results = join_all(futures).await;
    let final_ready = ready_count.load(Ordering::Relaxed);

    timer_handle.abort();

    if reveal_state.revealed.load(Ordering::Acquire) {
        let _ = detail_multi.clear();
    }

    let all_ready = results.iter().all(|&r| r);
    finalize_spinner(all_ready, &spinner_pb, final_ready, total);

    Ok(())
}

#[cfg(test)]
mod tests {
    use nostr::prelude::Keys;

    use super::*;
    use crate::git_http_auth::authorization_for_url;

    mod default_acceptance_maintainers {
        use nostr::prelude::{
            EventBuilder, Kind, Tag, event::FinalizeEvent, nip01::Coordinate,
            nip19::Nip19Coordinate,
        };

        use super::*;

        fn announcement(keys: &Keys, tags: Vec<Vec<String>>) -> Event {
            let mut event_tags = vec![Tag::identifier("test-repo")];
            for tag in tags {
                event_tags.push(Tag::parse(tag).unwrap());
            }
            EventBuilder::new(Kind::GitRepoAnnouncement, "")
                .tags(event_tags)
                .finalize(keys)
                .unwrap()
        }

        fn role(letter: &str, pk: &PublicKey) -> Vec<String> {
            vec![letter.to_string(), pk.to_string()]
        }

        /// Consolidate `events` the way `get_repo_ref_from_cache` would:
        /// the first event is the selected maintainer's, `maintainers` is
        /// the recursive union.
        fn repo_ref_from(events: Vec<Event>, maintainers: Vec<PublicKey>) -> RepoRef {
            let mut repo_ref = RepoRef::try_from((events[0].clone(), None)).unwrap();
            for event in events {
                repo_ref.events.insert(
                    Nip19Coordinate {
                        coordinate: Coordinate {
                            kind: Kind::GitRepoAnnouncement,
                            public_key: event.pubkey,
                            identifier: "test-repo".to_string(),
                        },
                        relays: vec![],
                    },
                    event,
                );
            }
            repo_ref.maintainers = maintainers;
            repo_ref
        }

        #[test]
        fn acceptance_lists_only_self_and_the_confirmed_lead() {
            let lead_keys = Keys::generate();
            let lead = lead_keys.public_key();
            let co_keys = Keys::generate();
            let co = co_keys.public_key();
            let me = Keys::generate().public_key();

            let repo_ref = repo_ref_from(
                vec![
                    announcement(
                        &lead_keys,
                        vec![role("M", &lead), role("m", &co), role("m", &me)],
                    ),
                    announcement(&co_keys, vec![role("M", &lead), role("m", &co)]),
                ],
                vec![lead, co, me],
            );

            assert_eq!(
                default_acceptance_maintainers(&repo_ref, me),
                vec![me, lead]
            );
        }

        #[test]
        fn the_accepting_lead_keeps_the_full_listing() {
            let selected_keys = Keys::generate();
            let selected = selected_keys.public_key();
            let lead = Keys::generate().public_key();
            let co = Keys::generate().public_key();

            // the selected maintainer designated another pubkey as lead;
            // that pubkey's acceptance announcement defines the membership,
            // so it lists everyone
            let repo_ref = repo_ref_from(
                vec![announcement(
                    &selected_keys,
                    vec![role("m", &selected), role("M", &lead), role("m", &co)],
                )],
                vec![selected, lead, co],
            );

            assert_eq!(
                default_acceptance_maintainers(&repo_ref, lead),
                vec![lead, selected, co]
            );
        }

        #[test]
        fn an_unconfirmed_lead_is_not_reciprocated_alone() {
            let selected_keys = Keys::generate();
            let selected = selected_keys.public_key();
            let lead = Keys::generate().public_key();
            let me = Keys::generate().public_key();

            // the designated lead has not announced: listing only them
            // could not acknowledge a confirmed member, so fall back to
            // the sole confirmed maintainer
            let repo_ref = repo_ref_from(
                vec![announcement(
                    &selected_keys,
                    vec![role("m", &selected), role("M", &lead), role("m", &me)],
                )],
                vec![selected, lead, me],
            );

            assert_eq!(
                default_acceptance_maintainers(&repo_ref, me),
                vec![me, selected]
            );
        }

        #[test]
        fn without_role_tags_the_sole_confirmed_maintainer_is_reciprocated() {
            let selected_keys = Keys::generate();
            let selected = selected_keys.public_key();
            let me = Keys::generate().public_key();

            let repo_ref = repo_ref_from(
                vec![announcement(
                    &selected_keys,
                    vec![vec!["maintainers".to_string(), me.to_string()]],
                )],
                vec![selected, me],
            );

            assert_eq!(
                default_acceptance_maintainers(&repo_ref, me),
                vec![me, selected]
            );
        }
    }

    #[tokio::test]
    async fn poll_authorization_is_only_installed_with_a_private_signer() {
        let signer = Arc::new(NgitSigner::Keys(Keys::generate()));
        let host = signer
            .get_public_key()
            .await
            .unwrap()
            .to_hex()
            .chars()
            .take(16)
            .collect::<String>();
        let public_url = format!("https://public-{host}.example/repo.git");
        let private_url = format!("https://private-{host}.example/repo.git");

        assert!(refresh_poll_authorization(&public_url, None).await);
        assert!(authorization_for_url(&public_url).is_none());

        assert!(refresh_poll_authorization(&private_url, Some(&signer)).await);
        assert!(authorization_for_url(&private_url).is_some());
    }
}
