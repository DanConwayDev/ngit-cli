//! Auto-accept co-maintainership on push.
//!
//! When a user has been offered co-maintainership (they appear in another
//! maintainer's `maintainers` tag but have never published their own
//! Kind:30617 announcement), pushing would normally fail. This module
//! provides `accept_maintainership_with_defaults`, called by the push path
//! to silently publish the co-maintainer's announcement with sensible
//! defaults before continuing the push.
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
use nostr::{
    PublicKey, ToBech32,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};
use nostr_sdk::{Kind, NostrSigner, RelayUrl};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::{Connect, send_events},
    git::{Repo, RepoActions},
    login::user::UserRef,
    repo_ref::{
        RepoRef, apply_grasp_infrastructure, format_grasp_server_url_as_clone_url,
        latest_event_repo_ref,
    },
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Publish the co-maintainer's own Kind:30617 announcement with defaults and
/// update the local git config / origin remote to point to it.
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
    signer: &Arc<dyn NostrSigner>,
) -> Result<()> {
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

    // --- Step 3: maintainers = [me, trusted_maintainer] ---

    let mut maintainers = vec![*my_pubkey];
    if repo_ref.trusted_maintainer != *my_pubkey {
        maintainers.push(repo_ref.trusted_maintainer);
    }

    // --- Step 4: build RepoRef ---

    let my_repo_ref = RepoRef {
        identifier: identifier.clone(),
        name: name.clone(),
        description,
        root_commit,
        git_server: git_servers,
        web,
        relays: relays.clone(),
        blossoms,
        hashtags,
        trusted_maintainer: *my_pubkey,
        maintainers_without_annoucnement: None,
        maintainers,
        events: HashMap::new(),
        nostr_git_url: None,
    };

    // --- Step 5: sign and publish the announcement ---

    eprintln!(
        "info: accepting co-maintainership of '{}' with defaults",
        name
    );
    eprintln!("info: publishing your repository announcement to nostr...");

    let repo_event = my_repo_ref.to_event(signer).await?;

    client.set_signer(signer.clone()).await;

    send_events(
        client,
        Some(git_repo.get_path()?),
        vec![repo_event],
        user_ref.relays.write(),
        relays.clone(),
        false, // no spinner — we are mid-push
        true,  // silent
    )
    .await
    .context("failed to publish co-maintainer announcement")?;

    // --- Step 6: wait for grasp server provisioning ---

    if !selected_grasp_servers.is_empty() {
        wait_for_grasp_servers(git_repo, &selected_grasp_servers, my_pubkey, identifier).await?;
    }

    // --- Step 7: update nostr.repo git config ---

    git_repo
        .save_git_config_item(
            "nostr.repo",
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: *my_pubkey,
                    identifier: identifier.clone(),
                },
                relays: vec![],
            }
            .to_bech32()?,
            false,
        )
        .context("failed to update nostr.repo git config")?;

    // --- Step 8: update origin remote ---

    let nostr_url = my_repo_ref.to_nostr_git_url(&Some(git_repo)).to_string();
    if git_repo.git_repo.find_remote("origin").is_ok() {
        git_repo
            .git_repo
            .remote_set_url("origin", &nostr_url)
            .context("failed to update origin remote")?;
    } else {
        git_repo
            .git_repo
            .remote("origin", &nostr_url)
            .context("failed to set origin remote")?;
    }

    eprintln!("info: co-maintainership accepted. run `ngit init` to customise your announcement.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Grasp server helpers
// ---------------------------------------------------------------------------

/// Return grasp servers for a co-maintainer using the following priority:
///
/// 1. User's own saved grasp server list (if non-empty).
/// 2. Trusted maintainer's grasp servers derived from
///    `trusted_maintainer_repo_ref` (if provided and non-empty). If the trusted
///    maintainer only uses a single grasp server, the first system-default
///    grasp server is appended so the co-maintainer has at least two servers
///    for redundancy.
/// 3. System / client default grasp servers.
pub fn grasp_servers_from_user_or_fallback(
    user_ref: &UserRef,
    trusted_maintainer_repo_ref: Option<&RepoRef>,
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

    // Priority 2: trusted maintainer's grasp servers.
    if let Some(rr) = trusted_maintainer_repo_ref {
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
}

fn check_git_server_ready(git_repo_path: &std::path::Path, git_server_url: &str) -> bool {
    let Ok(git_repo) = git2::Repository::open(git_repo_path) else {
        return false;
    };
    let Ok(mut remote) = git_repo.remote_anonymous(git_server_url) else {
        return false;
    };
    match remote.connect(git2::Direction::Fetch) {
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
