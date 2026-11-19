use std::{
    collections::HashMap,
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use auth_git2::GitAuthenticator;
use bitcoin_hashes::sha1::Hash as Sha1Hash;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};

use crate::{
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        remote_helper,
    },
    git_http_auth::{authorization_for_url, refresh_private_git_auth_for_url},
    output_mode::progress_policy,
    repo_ref::is_grasp_server_clone_url,
    repo_state::RepoState,
    signer::NgitSigner,
    utils::{
        Direction, get_read_protocols_to_try, get_short_git_server_name, join_with_and,
        onion_proxy_options_for_url, set_protocol_preference,
    },
};

const SPINNER_EXPAND_DELAY_SECS: u64 = 5;

/// Sync issues identified for a single remote
#[derive(Default, Debug, Clone)]
pub struct RemoteIssues {
    pub branches_out_of_sync: Vec<(String, Option<(usize, usize)>)>, // (ref, (ahead, behind))
    pub branches_missing: Vec<String>,
    pub tags_out_of_sync: Vec<String>,
    pub tags_missing: Vec<String>,
}

impl RemoteIssues {
    /// Returns true if there are no issues
    pub fn is_empty(&self) -> bool {
        self.branches_out_of_sync.is_empty()
            && self.branches_missing.is_empty()
            && self.tags_out_of_sync.is_empty()
            && self.tags_missing.is_empty()
    }

    /// Returns the total count of all issues
    pub fn total_count(&self) -> usize {
        self.branches_out_of_sync.len()
            + self.branches_missing.len()
            + self.tags_out_of_sync.len()
            + self.tags_missing.len()
    }
}

static GIT_SERVER_SUCCESS_THRESHOLD: f64 = 0.5; // 50% of servers must succeed to switch to short timeout

fn git_server_long_timeout() -> u64 {
    if std::env::var("NGITTEST").is_ok() {
        1
    } else {
        60
    }
}

fn git_server_short_timeout() -> u64 {
    if std::env::var("NGITTEST").is_ok() {
        1
    } else {
        5
    }
}

fn git_server_pb_style(current_timeout: Arc<AtomicU64>) -> Result<ProgressStyle> {
    Ok(ProgressStyle::with_template(" {spinner} {prefix} {msg}")?
        .with_key(
            "timeout_display",
            move |_state: &ProgressState, w: &mut dyn std::fmt::Write| {
                write!(w, "{}s", current_timeout.load(Ordering::Relaxed)).unwrap();
            },
        )
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "))
}

fn git_server_pb_after_style(succeed: bool) -> ProgressStyle {
    let symbol = if succeed {
        console::style("✔".to_string())
            .for_stderr()
            .green()
            .to_string()
    } else {
        console::style("✘".to_string())
            .for_stderr()
            .red()
            .to_string()
    };
    ProgressStyle::with_template(&format!(" {symbol} {{prefix}} {{msg}}"))
        .unwrap_or_else(|_| ProgressStyle::default_bar())
}

pub async fn list_from_remotes(
    term: &console::Term,
    git_repo: &Repo,
    git_servers: &[String],
    decoded_nostr_url: &NostrUrlDecoded,
    nostr_state: Option<&RepoState>,
    private_signer: Option<&Arc<NgitSigner>>,
) -> HashMap<String, (HashMap<String, String>, bool)> {
    if git_servers.is_empty() {
        return HashMap::new();
    }

    let policy = progress_policy();
    let quiet = policy.quiet;
    let progress_reporter = MultiProgress::with_draw_target(if policy.animate && policy.detailed {
        indicatif::ProgressDrawTarget::stderr()
    } else {
        indicatif::ProgressDrawTarget::hidden()
    });
    let spinner = (policy.animate && !policy.detailed).then(|| {
        let spinner = ProgressBar::new_spinner()
            .with_style(ProgressStyle::with_template("{spinner} {msg}").unwrap())
            .with_message("Checking git servers...");
        spinner.enable_steady_tick(Duration::from_millis(100));
        spinner
    });
    if !policy.animate && !quiet {
        eprintln!("Checking git servers...");
    }

    let bars = Arc::new(Mutex::new(Vec::<ProgressBar>::new()));
    let success_count = Arc::new(AtomicU64::new(0));
    let current_timeout = Arc::new(AtomicU64::new(git_server_long_timeout()));
    let total_servers = git_servers.len() as u64;

    let server_column_width = git_servers
        .iter()
        .map(|s| get_short_git_server_name(s).chars().count())
        .max()
        .unwrap_or(20)
        + 2;

    let futures: Vec<_> = git_servers
        .iter()
        .map(|url| {
            let url = url.clone();
            let is_grasp_server = is_grasp_server_clone_url(&url);
            let success_count_clone = success_count.clone();
            let current_timeout_clone = current_timeout.clone();
            let progress_reporter_clone = progress_reporter.clone();
            let bars = bars.clone();
            let decoded_nostr_url = decoded_nostr_url.clone();
            let private_signer = private_signer.cloned();

            async move {
                let server_name = get_short_git_server_name(&url);

                if !policy.animate && policy.detailed && !quiet {
                    eprintln!("{server_name}: connecting");
                }
                let pb = if policy.animate {
                    git_server_pb_style(current_timeout_clone.clone()).ok().map(|style| {
                        let pb = progress_reporter_clone.add(
                            ProgressBar::new(1)
                                .with_prefix(format!("{server_name:<server_column_width$} connecting"))
                                .with_style(style),
                        );
                        pb.enable_steady_tick(Duration::from_millis(300));
                        bars.lock().unwrap().push(pb.clone());
                        pb
                    })
                } else {
                    None
                };

                fn update_progress_bar_with_error(
                    server_column_width: usize,
                    server_name: &str,
                    pb: Option<ProgressBar>,
                    error: &anyhow::Error,
                ) {
                    if let Some(pb) = pb {
                        pb.set_style(git_server_pb_after_style(false));
                        pb.set_prefix(
                            console::style(format!("{: <server_column_width$}", server_name))
                                .for_stderr()
                                .dim()
                                .to_string(),
                        );
                        pb.disable_steady_tick();
                        pb.set_message(
                            console::style(error.to_string())
                                .for_stderr()
                                .red()
                                .to_string(),
                        );
                    }
                }

                if let Some(signer) = private_signer.as_ref() {
                    if let Err(error) = refresh_private_git_auth_for_url(&url, signer).await {
                        update_progress_bar_with_error(
                            server_column_width,
                            &server_name,
                            pb,
                            &error,
                        );
                        return Err((url, error));
                    }
                }

                let git_repo_path = Some(git_repo.workdir_or_git_dir().to_path_buf());
                let url_clone = url.clone();
                let decoded_nostr_url_clone = decoded_nostr_url.clone();
                let pb_clone = pb.clone();

                let list_future = async move {
                    match tokio::task::spawn_blocking(move || {
                        let git_repo = match git_repo_path {
                            Some(path) => Repo::from_path(&path).ok(),
                            None => None,
                        };

                        match git_repo {
                            Some(ref repo) => list_from_remote_sync(
                                repo,
                                &url_clone,
                                &decoded_nostr_url_clone,
                                is_grasp_server,
                                pb_clone.as_ref(),
                            ),
                            None => Err(anyhow!("failed to open git repository")),
                        }
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(e) => Err(anyhow!("task join error: {}", e)),
                    }
                };

                let timeout_future = async {
                    let check_interval = Duration::from_millis(100);
                    let long_timeout_end = tokio::time::Instant::now()
                        + Duration::from_secs(git_server_long_timeout());

                    loop {
                        let current_success_count = success_count_clone.load(Ordering::Relaxed);
                        let threshold = (total_servers as f64 * GIT_SERVER_SUCCESS_THRESHOLD).ceil() as u64;

                        if current_success_count >= threshold {
                            tokio::time::sleep(Duration::from_secs(git_server_short_timeout())).await;
                            return "short";
                        }

                        if tokio::time::Instant::now() >= long_timeout_end {
                            return "long";
                        }

                        tokio::time::sleep(check_interval).await;
                    }
                };

                let result = tokio::select! {
                    result = list_future => {
                        if result.is_ok() {
                            let new_count = success_count_clone.fetch_add(1, Ordering::Relaxed) + 1;
                            let threshold = (total_servers as f64 * GIT_SERVER_SUCCESS_THRESHOLD).ceil() as u64;

                            if new_count >= threshold {
                                current_timeout_clone.store(git_server_short_timeout(), Ordering::Relaxed);
                            }
                        }
                        result
                    }
                    timeout_type = timeout_future => {
                        Err(anyhow!("timeout after {}s",
                            if timeout_type == "long" { git_server_long_timeout() } else { git_server_short_timeout() }))
                    }
                };

                match result {
                    Err(error) => {
                        update_progress_bar_with_error(
                            server_column_width,
                            &server_name,
                            pb,
                            &error,
                        );
                        Err((url, error))
                    }
                    Ok(state) => {
                        let status_msg = if state.is_empty() {
                            "empty repository".to_string()
                        } else if let Some(nostr_state) = nostr_state {
                            let mut temp_states = HashMap::new();
                            temp_states.insert(url.clone(), (state.clone(), is_grasp_server));
                            let remote_issues = identify_remote_sync_issues(git_repo, nostr_state, &temp_states);
                            let warnings = generate_remote_sync_warnings(&remote_issues, &temp_states);

                            if warnings.is_empty() {
                                "in sync".to_string()
                            } else {
                                let warning = &warnings[0];
                                let server_name = get_short_git_server_name(&url);
                                let prefix = format!("WARNING: {} ", server_name);
                                warning.strip_prefix(&prefix)
                                    .unwrap_or(warning)
                                    .to_string()
                            }
                        } else {
                            "success".to_string()
                        };

                        let message_style = if status_msg == "empty repository"
                            || status_msg == "in sync"
                            || status_msg == "success"
                        {
                            console::style(&status_msg).for_stderr().green()
                        } else {
                            console::style(&status_msg).for_stderr().yellow()
                        };

                        if let Some(pb) = pb {
                            pb.set_style(git_server_pb_after_style(true));
                            pb.set_prefix(
                                console::style(format!("{: <server_column_width$}", server_name))
                                    .for_stderr()
                                    .dim()
                                    .to_string(),
                            );
                            pb.disable_steady_tick();
                            pb.set_message(message_style.to_string());
                        }
                        if !policy.animate && policy.detailed && !quiet {
                            eprintln!("{server_name}: {status_msg}");
                        }
                        Ok((url, state, is_grasp_server))
                    }
                }
            }
        })
        .collect();

    let work = stream::iter(futures)
        .buffer_unordered(15)
        .collect::<Vec<Result<(String, HashMap<String, String>, bool), (String, anyhow::Error)>>>();
    tokio::pin!(work);
    let results = tokio::select! {
        results = &mut work => results,
        _ = tokio::time::sleep(Duration::from_secs(SPINNER_EXPAND_DELAY_SECS)), if spinner.is_some() => {
            spinner.as_ref().unwrap().finish_and_clear();
            progress_reporter.set_draw_target(indicatif::ProgressDrawTarget::stderr());
            work.await
        }
    };

    let mut remote_states = HashMap::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok((url, state, is_grasp_server)) => {
                remote_states.insert(url, (state, is_grasp_server));
            }
            Err((url, error)) => {
                errors.push((url, error));
            }
        }
    }

    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }
    for bar in bars.lock().unwrap().iter() {
        bar.finish();
    }
    if policy.animate && !policy.detailed {
        let _ = progress_reporter.clear();
    }
    if !quiet {
        for (url, error) in &errors {
            let _ = term.write_line(&format!("failed to list from {url}: {error}"));
        }
    }
    if !policy.animate && !quiet {
        eprintln!(
            "{}",
            git_servers_summary(remote_states.len(), total_servers, !errors.is_empty())
        );
    }

    remote_states
}

fn git_servers_summary(responded: usize, total: u64, has_errors: bool) -> String {
    format!(
        "Git servers checked: {responded}/{total} responded{}.",
        if has_errors {
            "; some servers failed"
        } else {
            ""
        }
    )
}

// Backward-compatible synchronous wrapper for use in non-async contexts
pub fn list_from_remote(
    _term: &console::Term,
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    is_grasp_server: bool,
) -> Result<HashMap<String, String>> {
    list_from_remote_sync(
        git_repo,
        git_server_url,
        decoded_nostr_url,
        is_grasp_server,
        None,
    )
}

fn list_from_remote_sync(
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    is_grasp_server: bool,
    pb: Option<&ProgressBar>,
) -> Result<HashMap<String, String>> {
    remote_helper::validate_clone_url(git_server_url)?;

    if remote_helper::handles_url(git_server_url) {
        if let Some(pb) = pb {
            pb.set_message("via Git remote helper");
        }
        return remote_helper::list(git_repo, git_server_url, false);
    }

    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_read_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

    let mut failed_protocols = vec![];
    let mut last_failure = None;
    let mut remote_state: Option<HashMap<String, String>> = None;

    for protocol in &protocols_to_attempt {
        if let Some(pb) = pb {
            // Only show protocol for non-grasp servers as they can failover to other
            // protocols
            if is_grasp_server {
                pb.set_message("".to_string());
            } else {
                pb.set_message(format!("via {protocol}"));
            }
        }

        let formatted_url = server_url.format_as(protocol)?;

        let res = list_from_remote_url(
            git_repo,
            &formatted_url,
            decoded_nostr_url.ssh_key_file_path().as_ref(),
            [ServerProtocol::UnauthHttps, ServerProtocol::UnauthHttp].contains(protocol),
        );

        match res {
            Ok(state) => {
                remote_state = Some(state);
                if !is_grasp_server && !failed_protocols.is_empty() {
                    let _ =
                        set_protocol_preference(git_repo, protocol, &server_url, &Direction::Fetch);
                }
                break;
            }
            Err(error) => {
                failed_protocols.push(protocol);
                if failed_protocols.len() == protocols_to_attempt.len() {
                    // All protocols failed
                    if let Some(pb) = pb {
                        pb.set_message(format!("all protocols failed: {}", error));
                    }
                    last_failure = Some(error);
                }
            }
        }
    }

    if let Some(remote_state) = remote_state {
        Ok(remote_state)
    } else {
        let error = anyhow!(
            "{} failed over {}{}",
            server_url.short_name(),
            join_with_and(&failed_protocols),
            if decoded_nostr_url.protocol.is_some() {
                " and nostr url contains protocol override so no other protocols were attempted"
            } else {
                ""
            },
        );
        match last_failure {
            Some(cause) => Err(anyhow!("{error}: {cause:#}")),
            None => Err(error),
        }
    }
}

fn list_from_remote_url(
    git_repo: &Repo,
    git_server_remote_url: &str,
    ssh_key_file: Option<&String>,
    dont_authenticate: bool,
) -> Result<HashMap<String, String>> {
    if remote_helper::is_onion_http(git_server_remote_url) {
        return remote_helper::list(git_repo, git_server_remote_url, dont_authenticate);
    }
    let git_config = git_repo.git_repo.config()?;

    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_remote_url)?;
    // authentication may be required
    let auth = {
        if dont_authenticate {
            GitAuthenticator::default()
        } else if git_server_remote_url.contains("git@") {
            if let Some(ssh_key_file) = ssh_key_file {
                GitAuthenticator::default()
                    .add_ssh_key_from_file(PathBuf::from_str(ssh_key_file)?, None)
            } else {
                GitAuthenticator::default()
            }
        } else {
            GitAuthenticator::default()
        }
    };
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    if !dont_authenticate {
        remote_callbacks.credentials(auth.credentials(&git_config));
    }
    let mut fetch_options = git2::FetchOptions::new();
    fetch_options.remote_callbacks(remote_callbacks);
    if let Some(proxy) = onion_proxy_options_for_url(git_server_remote_url)? {
        fetch_options.proxy_options(proxy);
    }
    let authorization = authorization_for_url(git_server_remote_url);
    if let Some(header) = authorization.as_deref() {
        fetch_options.custom_headers(&[header]);
    }
    // `connect_auth` cannot carry the NIP-98 header. Use download to obtain
    // the advertisement, but exclude every ref and disable automatic tags:
    // an empty refspec list implicitly fetches HEAD and its entire history.
    // Listing servers must not download packs into the shared object database.
    fetch_options.download_tags(git2::AutotagOption::None);
    git_server_remote.download(&["^*"], Some(&mut fetch_options))?;
    let mut state = HashMap::new();
    for head in git_server_remote.list()? {
        if let Some(symbolic_reference) = head.symref_target() {
            state.insert(
                head.name().to_string(),
                format!("ref: {symbolic_reference}"),
            );
        // ignore dereferenced tags
        } else if !head.name().to_string().ends_with("^{}") {
            state.insert(head.name().to_string(), head.oid().to_string());
        }
    }
    git_server_remote.disconnect()?;
    Ok(state)
}

pub fn get_ahead_behind(
    git_repo: &Repo,
    base_ref_or_oid: &str,
    latest_ref_or_oid: &str,
) -> Result<(Vec<Sha1Hash>, Vec<Sha1Hash>)> {
    let base = git_repo.get_commit_or_tip_of_reference(base_ref_or_oid)?;
    let latest = git_repo.get_commit_or_tip_of_reference(latest_ref_or_oid)?;
    git_repo.get_commits_ahead_behind(&base, &latest)
}

/// Identify sync discrepancies between nostr state and remote git servers
///
/// This function analyzes the differences between the expected state (from
/// nostr) and the actual state on each remote git server, categorizing issues
/// by type (branches/tags, out of sync/missing).
///
/// # Arguments
/// * `git_repo` - The local git repository
/// * `nostr_state` - The expected state from nostr
/// * `remote_states` - Map of remote URLs to their states and whether they're
///   grasp servers
///
/// # Returns
/// A HashMap mapping remote names to their identified sync issues
pub fn identify_remote_sync_issues(
    git_repo: &Repo,
    nostr_state: &RepoState,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
) -> HashMap<String, RemoteIssues> {
    let mut remote_issues: HashMap<String, RemoteIssues> = HashMap::new();

    for (name, value) in &nostr_state.state {
        // skip peeled-tag dereference markers — not real refs
        if name.ends_with("^{}") {
            continue;
        }
        for (url, (remote_state, _is_grasp_server)) in remote_states {
            let remote_name = get_short_git_server_name(url);
            let issues = remote_issues.entry(remote_name.clone()).or_default();

            let is_branch = name.starts_with("refs/heads/");
            let is_tag = name.starts_with("refs/tags/");

            if let Some(remote_value) = remote_state.get(name) {
                if value.ne(remote_value) {
                    if is_branch {
                        // Calculate ahead/behind for branches
                        let ahead_behind = get_ahead_behind(git_repo, value, remote_value)
                            .ok()
                            .map(|(ahead, behind)| (ahead.len(), behind.len()));
                        issues
                            .branches_out_of_sync
                            .push((name.clone(), ahead_behind));
                    } else if is_tag {
                        issues.tags_out_of_sync.push(name.clone());
                    }
                }
            } else if is_branch {
                issues.branches_missing.push(name.clone());
            } else if is_tag {
                issues.tags_missing.push(name.clone());
            }
        }
    }

    remote_issues
}

/// Format a list of refs with ahead/behind information into a user-friendly
/// issue summary
///
/// # Arguments
/// * `refs` - List of refs with optional ahead/behind counts
/// * `singular` - Singular form of the ref type (e.g., "branch")
/// * `plural` - Plural form of the ref type (e.g., "branches")
/// * `status` - Status description (e.g., "out of sync", "missing")
/// * `is_branch` - Whether these are branches (affects formatting)
///
/// # Returns
/// A formatted string describing the issue
pub fn format_ref_issue(
    refs: &[(String, Option<(usize, usize)>)],
    _singular: &str,
    plural: &str,
    status: &str,
    is_branch: bool,
) -> String {
    let count = refs.len();

    /// Helper to format branch name with ahead/behind info
    fn format_branch_with_sync(name: &str, ahead_behind: &Option<(usize, usize)>) -> String {
        if let Some((ahead, behind)) = ahead_behind {
            if *ahead > 0 && *behind > 0 {
                format!("{} ({} behind, {} ahead)", name, behind, ahead)
            } else if *behind > 0 {
                format!("{} ({} behind)", name, behind)
            } else if *ahead > 0 {
                format!("{} ({} ahead)", name, ahead)
            } else {
                name.to_string()
            }
        } else {
            name.to_string()
        }
    }

    if count == 1 {
        // Single item: name the ref with ahead/behind info
        let clean_ref = refs[0]
            .0
            .strip_prefix("refs/heads/")
            .or_else(|| refs[0].0.strip_prefix("refs/tags/"))
            .unwrap_or(&refs[0].0);
        let formatted = if is_branch {
            format_branch_with_sync(clean_ref, &refs[0].1)
        } else {
            clean_ref.to_string()
        };
        format!("{} {}", formatted, status)
    } else if is_branch && count <= 3 {
        // For branches: list up to 3 names with ahead/behind info
        let names: Vec<_> = refs
            .iter()
            .map(|(r, ab)| {
                let clean = r.strip_prefix("refs/heads/").unwrap_or(r);
                format_branch_with_sync(clean, ab)
            })
            .collect();
        if count == 2 {
            format!("{} and {} {}", names[0], names[1], status)
        } else {
            format!("{}, {} and {} {}", names[0], names[1], names[2], status)
        }
    } else if is_branch && count > 3 {
        // For many branches: list first 2 with ahead/behind and count others
        let names: Vec<_> = refs
            .iter()
            .take(2)
            .map(|(r, ab)| {
                let clean = r.strip_prefix("refs/heads/").unwrap_or(r);
                format_branch_with_sync(clean, ab)
            })
            .collect();
        let other_count = count - 2;
        let other = if other_count == 1 {
            "1 other".to_string()
        } else {
            format!("{} others", other_count)
        };
        format!("{}, {} and {} {}", names[0], names[1], other, status)
    } else {
        // For tags: just count
        format!("{} {} {}", count, plural, status)
    }
}

/// Format a list of refs (String only) into a user-friendly issue summary
///
/// # Arguments
/// * `refs` - List of ref names
/// * `singular` - Singular form of the ref type (e.g., "branch")
/// * `plural` - Plural form of the ref type (e.g., "branches")
/// * `status` - Status description (e.g., "out of sync", "missing")
/// * `is_branch` - Whether these are branches (affects formatting)
///
/// # Returns
/// A formatted string describing the issue
pub fn format_ref_issue_simple(
    refs: &[String],
    _singular: &str,
    plural: &str,
    status: &str,
    is_branch: bool,
) -> String {
    let count = refs.len();

    if count == 1 {
        // Single item: name the ref
        let clean_ref = refs[0]
            .strip_prefix("refs/heads/")
            .or_else(|| refs[0].strip_prefix("refs/tags/"))
            .unwrap_or(&refs[0]);
        format!("{} {}", clean_ref, status)
    } else if is_branch && count <= 3 {
        // For branches: list up to 3 names
        let names: Vec<_> = refs
            .iter()
            .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r))
            .collect();
        if count == 2 {
            format!("{} and {} {}", names[0], names[1], status)
        } else {
            format!("{}, {} and {} {}", names[0], names[1], names[2], status)
        }
    } else if is_branch && count > 3 {
        // For many branches: list first 2 and count others
        let names: Vec<_> = refs
            .iter()
            .take(2)
            .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r))
            .collect();
        let other_count = count - 2;
        let other = if other_count == 1 {
            "1 other".to_string()
        } else {
            format!("{} others", other_count)
        };
        format!("{}, {} and {} {}", names[0], names[1], other, status)
    } else {
        // For tags: just count
        format!("{} {} {}", count, plural, status)
    }
}

/// Generate warning messages for remote sync issues
pub fn generate_remote_sync_warnings(
    remote_issues: &HashMap<String, RemoteIssues>,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
) -> Vec<String> {
    let mut warnings = Vec::new();

    for (remote_name, issues) in remote_issues {
        if issues.is_empty() {
            continue;
        }

        // Find remote state for this remote
        let remote_state = remote_states
            .iter()
            .find(|(url, _)| &get_short_git_server_name(url) == remote_name)
            .map(|(_, (state, _))| state);

        if let Some(state) = remote_state {
            // Check if remote is completely empty
            if state.is_empty() {
                warnings.push(format!("WARNING: {remote_name} has empty repository."));
                continue;
            }

            // Check if remote only has a few branches and missing many
            let remote_branches: Vec<_> = state
                .keys()
                .filter(|k| k.starts_with("refs/heads/"))
                .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
                .collect();

            if remote_branches.len() <= 3 && issues.branches_missing.len() >= 5 {
                let sync_status = if issues.branches_out_of_sync.is_empty() {
                    ""
                } else {
                    " and they are out of sync"
                };

                warnings.push(format!(
                    "WARNING: {remote_name} only has {} branches{}",
                    remote_branches.join(", "),
                    sync_status
                ));
                continue;
            }
        }

        // Build summary message parts
        let mut parts = Vec::new();

        if !issues.branches_out_of_sync.is_empty() {
            parts.push(format_ref_issue(
                &issues.branches_out_of_sync,
                "branch",
                "branches",
                "out of sync",
                true,
            ));
        }

        if !issues.branches_missing.is_empty() {
            parts.push(format_ref_issue_simple(
                &issues.branches_missing,
                "branch",
                "branches",
                "missing",
                true,
            ));
        }

        if !issues.tags_out_of_sync.is_empty() {
            parts.push(format_ref_issue_simple(
                &issues.tags_out_of_sync,
                "tag",
                "tags",
                "out of sync",
                false,
            ));
        }

        if !issues.tags_missing.is_empty() {
            parts.push(format_ref_issue_simple(
                &issues.tags_missing,
                "tag",
                "tags",
                "missing",
                false,
            ));
        }

        if !parts.is_empty() {
            warnings.push(format!(
                "WARNING: {remote_name} is out of sync. {}",
                parts.join(". ")
            ));
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    #[test]
    fn static_git_server_summary_preserves_partial_success() {
        let partial = super::git_servers_summary(2, 3, true);
        assert!(partial.contains("2/3"));
        assert!(partial.contains("failed"));
        let complete = super::git_servers_summary(3, 3, false);
        assert!(complete.contains("3/3"));
        assert!(!complete.contains("failed"));
    }

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn listing_refs_does_not_download_objects() -> Result<()> {
        let source_dir = tempfile::tempdir()?;
        let source = git2::Repository::init_bare(source_dir.path())?;
        source.set_head("refs/heads/main")?;
        let tree_id = source.treebuilder(None)?.write()?;
        let tree = source.find_tree(tree_id)?;
        let signature = git2::Signature::now("Test", "test@example.com")?;
        let commit = source.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])?;
        let object = source.find_object(commit, None)?;
        source.branch("feature", &source.find_commit(commit)?, false)?;
        let tag = source.tag("v1", &object, &signature, "release", false)?;

        let file_url = reqwest::Url::from_directory_path(source_dir.path()).unwrap();
        let server = test_harness::VanillaGitServer::start(
            "list-refs",
            test_harness::port::reserve_port()?,
            source_dir.path(),
        )
        .await?;
        for url in [file_url.as_str(), server.url()] {
            let destination_dir = tempfile::tempdir()?;
            let destination = Repo {
                git_repo: git2::Repository::init(destination_dir.path())?,
            };
            let refs = list_from_remote_url(&destination, url, None, true)?;
            assert_eq!(refs.get("HEAD"), Some(&"ref: refs/heads/main".to_string()));
            assert_eq!(refs.get("refs/heads/main"), Some(&commit.to_string()));
            assert_eq!(refs.get("refs/heads/feature"), Some(&commit.to_string()));
            assert_eq!(refs.get("refs/tags/v1"), Some(&tag.to_string()));
            assert!(!refs.contains_key("refs/tags/v1^{}"));
            let mut object_count = 0;
            destination.git_repo.odb()?.foreach(|_| {
                object_count += 1;
                true
            })?;
            assert_eq!(object_count, 0, "listing refs must not fetch Git objects");
            assert_eq!(destination.git_repo.references()?.count(), 0);
        }
        Ok(())
    }

    #[test]
    fn test_format_ref_issue_single_branch_with_ahead_behind() {
        let refs = vec![("refs/heads/main".to_string(), Some((5, 3)))];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (3 behind, 5 ahead) out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_single_branch_only_behind() {
        let refs = vec![("refs/heads/feature".to_string(), Some((0, 7)))];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "feature (7 behind) out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_single_branch_only_ahead() {
        let refs = vec![("refs/heads/dev".to_string(), Some((4, 0)))];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "dev (4 ahead) out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_single_branch_no_diff() {
        let refs = vec![("refs/heads/main".to_string(), Some((0, 0)))];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_single_branch_no_ahead_behind_info() {
        let refs = vec![("refs/heads/main".to_string(), None)];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_two_branches() {
        let refs = vec![
            ("refs/heads/main".to_string(), Some((2, 1))),
            ("refs/heads/dev".to_string(), Some((0, 3))),
        ];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (1 behind, 2 ahead) and dev (3 behind) out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_three_branches() {
        let refs = vec![
            ("refs/heads/main".to_string(), Some((1, 0))),
            ("refs/heads/dev".to_string(), Some((0, 2))),
            ("refs/heads/feature".to_string(), None),
        ];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (1 ahead), dev (2 behind) and feature out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_many_branches() {
        let refs = vec![
            ("refs/heads/main".to_string(), Some((5, 3))),
            ("refs/heads/dev".to_string(), Some((0, 1))),
            ("refs/heads/feature1".to_string(), None),
            ("refs/heads/feature2".to_string(), Some((2, 0))),
        ];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (3 behind, 5 ahead), dev (1 behind) and 2 others out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_many_branches_singular_other() {
        let refs = vec![
            ("refs/heads/main".to_string(), Some((1, 1))),
            ("refs/heads/dev".to_string(), Some((2, 2))),
            ("refs/heads/feature".to_string(), None),
        ];
        // With 3 branches, it should list all 3
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (1 behind, 1 ahead), dev (2 behind, 2 ahead) and feature out of sync"
        );

        // With 4 branches (show 2, then "2 others")
        let refs = vec![
            ("refs/heads/main".to_string(), Some((1, 1))),
            ("refs/heads/dev".to_string(), Some((2, 2))),
            ("refs/heads/feature1".to_string(), None),
            ("refs/heads/feature2".to_string(), None),
        ];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (1 behind, 1 ahead), dev (2 behind, 2 ahead) and 2 others out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_single_tag() {
        let refs = vec![("refs/tags/v1.0.0".to_string(), None)];
        assert_eq!(
            format_ref_issue(&refs, "tag", "tags", "out of sync", false),
            "v1.0.0 out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_multiple_tags() {
        let refs = vec![
            ("refs/tags/v1.0.0".to_string(), None),
            ("refs/tags/v1.0.1".to_string(), None),
            ("refs/tags/v2.0.0".to_string(), None),
        ];
        assert_eq!(
            format_ref_issue(&refs, "tag", "tags", "out of sync", false),
            "3 tags out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_single_branch() {
        let refs = vec!["refs/heads/main".to_string()];
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main missing"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_two_branches() {
        let refs = vec!["refs/heads/main".to_string(), "refs/heads/dev".to_string()];
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main and dev missing"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_three_branches() {
        let refs = vec![
            "refs/heads/main".to_string(),
            "refs/heads/dev".to_string(),
            "refs/heads/feature".to_string(),
        ];
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main, dev and feature missing"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_many_branches() {
        let refs = vec![
            "refs/heads/main".to_string(),
            "refs/heads/dev".to_string(),
            "refs/heads/feature1".to_string(),
            "refs/heads/feature2".to_string(),
        ];
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main, dev and 2 others missing"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_many_branches_singular_other() {
        let refs = vec![
            "refs/heads/main".to_string(),
            "refs/heads/dev".to_string(),
            "refs/heads/feature".to_string(),
            "refs/heads/hotfix".to_string(),
        ];
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main, dev and 2 others missing"
        );

        // Test with exactly 4 branches (2 shown + 2 others)
        let refs = vec![
            "refs/heads/main".to_string(),
            "refs/heads/dev".to_string(),
            "refs/heads/feature".to_string(),
        ];
        // With 3 branches, all should be shown
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main, dev and feature missing"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_single_tag() {
        let refs = vec!["refs/tags/v1.0.0".to_string()];
        assert_eq!(
            format_ref_issue_simple(&refs, "tag", "tags", "missing", false),
            "v1.0.0 missing"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_multiple_tags() {
        let refs = vec![
            "refs/tags/v1.0.0".to_string(),
            "refs/tags/v1.0.1".to_string(),
            "refs/tags/v2.0.0".to_string(),
        ];
        assert_eq!(
            format_ref_issue_simple(&refs, "tag", "tags", "missing", false),
            "3 tags missing"
        );
    }

    #[test]
    fn test_format_ref_issue_without_refs_prefix() {
        let refs = vec![("main".to_string(), Some((1, 0)))];
        assert_eq!(
            format_ref_issue(&refs, "branch", "branches", "out of sync", true),
            "main (1 ahead) out of sync"
        );
    }

    #[test]
    fn test_format_ref_issue_simple_without_refs_prefix() {
        let refs = vec!["main".to_string()];
        assert_eq!(
            format_ref_issue_simple(&refs, "branch", "branches", "missing", true),
            "main missing"
        );
    }
}
