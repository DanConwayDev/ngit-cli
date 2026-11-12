use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use auth_git2::GitAuthenticator;
use bitcoin_hashes::sha1::Hash as Sha1Hash;
use console::Term;
use nostr::prelude::{
    RelayUrl,
    event::{Event, EventBuilder, Kind, Tag, UnsignedEvent},
    key::PublicKey,
};

use crate::{
    cli_interactor::count_lines_per_msg_vec,
    client::{
        Connect, EarlyPublicationError, ProposalPublicationReceipt,
        get_all_proposal_patch_pr_pr_update_events_from_cache, publication_targets,
        save_event_in_local_cache, sign_draft_event,
    },
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        oid_to_shorthand_string, remote_helper,
    },
    git_events::generate_unsigned_pr_or_update_event,
    git_http_auth::{authorization_for_url, prepare_private_git_auth},
    login::user::UserRef,
    output_mode::{ProgressPolicy, is_quiet, progress_policy, write_progress_line},
    repo_ref::{
        RepoRef, extract_npub, format_grasp_server_url_as_grasp06_prs_url,
        format_grasp_server_url_as_relay_url, is_grasp_server_clone_url, is_grasp_server_in_list,
        normalize_grasp_server_url,
    },
    utils::{
        Direction, get_short_git_server_name, get_write_protocols_to_try,
        is_libgit2_internal_error, join_with_and, onion_proxy_options_for_url,
        set_protocol_preference,
    },
};

#[derive(Clone, Debug, Default)]
pub struct ProposalMetadata {
    pub target_branch: Option<String>,
    pub explicit_base: Option<Sha1Hash>,
}

// returns a HashMap of refs responded to and any related cancellation reasons
pub fn push_to_remote(
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    remote_refspecs: &[String],
    term: &Term,
    is_grasp_server: bool,
    git_server_push_options: &[&str],
) -> Result<HashMap<String, Option<String>>> {
    remote_helper::validate_clone_url(git_server_url)?;

    if remote_helper::handles_url(git_server_url) {
        write_progress_line(
            term,
            &format!("push: {git_server_url} via Git remote helper..."),
        )?;
        return remote_helper::push(
            git_repo,
            git_server_url,
            remote_refspecs,
            term,
            git_server_push_options,
            decoded_nostr_url.ssh_key_file_path().as_deref(),
        );
    }

    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_write_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

    let mut failed_protocols = vec![];
    let mut success = false;
    let mut ref_updates = HashMap::new();

    for protocol in &protocols_to_attempt {
        write_progress_line(
            term,
            format!("push: {} over {protocol}...", server_url.short_name()).as_str(),
        )?;

        let formatted_url = server_url.format_as(protocol)?;

        let attempt = push_to_remote_url(
            git_repo,
            &formatted_url,
            decoded_nostr_url.ssh_key_file_path().as_ref(),
            remote_refspecs,
            term,
            git_server_push_options,
        )
        .or_else(|error| {
            // A libgit2 internal assertion means libgit2 itself cannot talk
            // to this server (e.g. tangled.org's ref advertisement trips
            // `GIT_ASSERT(eos)` in libgit2 1.9), not that the push was
            // rejected, so retry the same URL with the system git binary.
            if !is_libgit2_internal_error(&error) {
                return Err(error);
            }
            write_progress_line(
                term,
                format!(
                    "push: libgit2 failed over {protocol} with an internal error ({error}); retrying with system git..."
                )
                .as_str(),
            )?;
            remote_helper::push(
                git_repo,
                &formatted_url,
                remote_refspecs,
                term,
                git_server_push_options,
                decoded_nostr_url.ssh_key_file_path().as_deref(),
            )
        });

        match attempt {
            Err(error) => {
                write_progress_line(
                    term,
                    format!(
                        "push: {formatted_url} failed over {protocol}{}: {error}",
                        if protocol == &ServerProtocol::Ssh {
                            if let Some(ssh_key_file) = &decoded_nostr_url.ssh_key_file_path() {
                                format!(" with ssh key from {ssh_key_file}")
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    )
                    .as_str(),
                )?;
                failed_protocols.push(protocol);
            }
            Ok(ref_updates_on_protocol) => {
                success = true;
                if ref_updates_on_protocol
                    .values()
                    .all(|error| error.is_none())
                {
                    if !failed_protocols.is_empty() {
                        write_progress_line(
                            term,
                            format!("push: succeeded over {protocol}").as_str(),
                        )?;
                        let _ = set_protocol_preference(
                            git_repo,
                            protocol,
                            &server_url,
                            &Direction::Push,
                        );
                    }
                    break;
                } else {
                    term.write_line(
                        format!(
                            "push: {formatted_url} with {protocol} complete but {}ref{} not accepted:", 
                            if remote_refspecs.len() != failed_protocols.len() { "some " } else {""},
                            if remote_refspecs.len() == 1 { "s"} else {""},
                        ).as_str(),
                    )?;
                    for (git_ref, error) in &ref_updates_on_protocol {
                        if let Some(error) = error {
                            term.write_line(format!("push:    - {git_ref}: {error}").as_str())?;
                        }
                    }
                    // TODO do we want to report on the refs that weren't responded to?
                    ref_updates = ref_updates_on_protocol;
                }
                break;
            }
        }
    }
    if success {
        Ok(ref_updates)
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
        Err(error)
    }
}

// returns HashMaps of refspecs responded to and any failure message
pub fn push_to_remote_url(
    git_repo: &Repo,
    git_server_url: &str,
    ssh_key_file: Option<&String>,
    remote_refspecs: &[String],
    term: &Term,
    git_server_push_options: &[&str],
) -> Result<HashMap<String, Option<String>>> {
    let git_config = git_repo.git_repo.config()?;
    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_url)?;
    let auth = {
        if git_server_url.contains("git@") {
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
    let mut push_options = git2::PushOptions::new();
    if let Some(proxy) = onion_proxy_options_for_url(git_server_url)? {
        push_options.proxy_options(proxy);
    }
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    let push_reporter = Arc::new(Mutex::new(PushReporter::new(term)));

    remote_callbacks.credentials(auth.credentials(&git_config));

    remote_callbacks.push_update_reference({
        let push_reporter = Arc::clone(&push_reporter);
        move |name, error| {
            let mut reporter = push_reporter.lock().unwrap();
            reporter
                .ref_updates
                .insert(name.to_string(), error.map(|s| s.to_string()));
            if let Some(error) = error {
                let existing_lines = reporter.count_all_existing_lines();
                reporter.update_reference_errors.push(format!(
                    "WARNING: {} failed to push {name} error: {error}",
                    get_short_git_server_name(git_server_url),
                ));
                reporter.write_all(existing_lines);
            }
            Ok(())
        }
    });

    remote_callbacks.push_negotiation({
        let push_reporter = Arc::clone(&push_reporter);
        move |updates| {
            let mut reporter = push_reporter.lock().unwrap();
            let existing_lines = reporter.count_all_existing_lines();

            for update in updates {
                let dst_refname = update
                    .dst_refname()
                    .unwrap_or("")
                    .replace("refs/heads/", "")
                    .replace("refs/tags/", "tags/");
                let msg = if let Some(Some(_)) =
                    reporter.ref_updates.get(update.dst_refname().unwrap_or(""))
                {
                    format!("push: - [failed]          {dst_refname}")
                } else if update.dst().is_zero() {
                    format!("push: - [delete]          {dst_refname}")
                } else if update.src().is_zero() {
                    if update.dst_refname().unwrap_or("").contains("refs/tags") {
                        format!("push: * [new tag]         {dst_refname}")
                    } else {
                        format!("push: * [new branch]      {dst_refname}")
                    }
                } else {
                    let force = remote_refspecs
                        .iter()
                        .any(|r| r.contains(&dst_refname) && r.contains('+'));
                    format!(
                        "push: {} {}..{}  {} -> {dst_refname}",
                        if force { "+" } else { " " },
                        oid_to_shorthand_string(update.src()).unwrap(),
                        oid_to_shorthand_string(update.dst()).unwrap(),
                        update
                            .src_refname()
                            .unwrap_or("")
                            .replace("refs/heads/", "")
                            .replace("refs/tags/", "tags/"),
                    )
                };
                // other possibilities will result in push to fail but better reporting is
                // needed:
                // deleting a non-existant branch:
                // ! [remote rejected] <old-branch-name> -> <old-branch-name> (not found)
                // adding a branch that already exists:
                // ! [remote rejected] <new-branch-name> -> <new-branch-name> (already exists)
                // pushing without non-fast-forward without force:
                // ! [rejected]        <branch-name> -> <branch-name> (non-fast-forward)
                reporter.negotiation.push(msg);
            }
            reporter.write_all(existing_lines);
            Ok(())
        }
    });

    remote_callbacks.push_transfer_progress({
        let push_reporter = Arc::clone(&push_reporter);
        #[allow(clippy::cast_precision_loss)]
        move |current, total, bytes| {
            let mut reporter = push_reporter.lock().unwrap();
            reporter.process_transfer_progress_update(current, total, bytes);
        }
    });

    remote_callbacks.sideband_progress({
        let push_reporter = Arc::clone(&push_reporter);
        move |data| {
            let mut reporter = push_reporter.lock().unwrap();
            reporter.process_remote_msg(data);
            true
        }
    });
    push_options.remote_callbacks(remote_callbacks);
    let authorization = authorization_for_url(git_server_url);
    if let Some(header) = authorization.as_deref() {
        push_options.custom_headers(&[header]);
    }
    if !git_server_push_options.is_empty() {
        push_options.remote_push_options(git_server_push_options);
    }
    git_server_remote.push(remote_refspecs, Some(&mut push_options))?;
    let _ = git_server_remote.disconnect();
    let reporter = push_reporter.lock().unwrap();
    Ok(reporter.ref_updates.clone())
}

#[allow(clippy::cast_precision_loss)]
#[allow(clippy::float_cmp)]
#[allow(clippy::needless_pass_by_value)]
fn report_on_transfer_progress(
    current: usize,
    total: usize,
    bytes: usize,
    start_time: &Instant,
    end_time: Option<&Instant>,
) -> Option<String> {
    if total == 0 {
        return None;
    }
    let percentage = ((current as f64 / total as f64) * 100.0)
        // always round down because 100% complete is misleading when its not complete
        .floor();
    let (size, unit) = if bytes as f64 >= (1024.0 * 1024.0) {
        (bytes as f64 / (1024.0 * 1024.0), "MiB")
    } else {
        (bytes as f64 / 1024.0, "KiB")
    };
    let speed = {
        let duration = if let Some(end_time) = end_time {
            (*end_time - *start_time).as_millis() as f64
        } else {
            start_time.elapsed().as_millis() as f64
        };

        if duration > 0.0 {
            (bytes as f64 / (1024.0 * 1024.0)) / (duration / 1000.0) // Convert bytes to MiB and milliseconds to seconds
        } else {
            0.0
        }
    };

    Some(format!(
        "push: Writing objects: {percentage}% ({current}/{total}) {size:.2} {unit}  | {speed:.2} MiB/s{}",
        if current == total { ", done." } else { "" },
    ))
}

pub struct PushReporter<'a> {
    remote_msgs: Vec<String>,
    negotiation: Vec<String>,
    transfer_progress_msgs: Vec<String>,
    update_reference_errors: Vec<String>,
    ref_updates: HashMap<String, Option<String>>,
    policy: ProgressPolicy,
    logged: HashSet<String>,
    term: &'a console::Term,
    start_time: Option<Instant>,
    end_time: Option<Instant>,
}
impl<'a> PushReporter<'a> {
    fn new(term: &'a console::Term) -> Self {
        Self {
            remote_msgs: vec![],
            negotiation: vec![],
            transfer_progress_msgs: vec![],
            update_reference_errors: vec![],
            ref_updates: HashMap::new(),
            term,
            policy: progress_policy(),
            logged: HashSet::new(),
            start_time: None,
            end_time: None,
        }
    }
    fn write_all(&mut self, lines_to_clear: usize) {
        if is_quiet() {
            return;
        }
        if !self.policy.animate {
            self.write_log();
            return;
        }
        let _ = self.term.clear_last_lines(lines_to_clear);
        for msg in &self.remote_msgs {
            let _ = self.term.write_line(format!("remote: {msg}").as_str());
        }
        for msg in &self.negotiation {
            let _ = self.term.write_line(msg);
        }
        for msg in &self.transfer_progress_msgs {
            let _ = self.term.write_line(msg);
        }
        for msg in &self.update_reference_errors {
            let _ = self.term.write_line(msg);
        }
    }

    fn write_log(&mut self) {
        if self.policy.detailed {
            for message in &self.remote_msgs {
                if !message.contains('%') && self.logged.insert(format!("remote:{message}")) {
                    let _ = self.term.write_line(&format!("remote: {message}"));
                }
            }
            for message in &self.transfer_progress_msgs {
                // Keep one completion per phase, not changing byte/speed samples.
                let phase = message.split('%').next().unwrap_or(message);
                if message.ends_with(", done.") && self.logged.insert(phase.to_owned()) {
                    let _ = self.term.write_line(message);
                }
            }
            for message in &self.negotiation {
                if self.logged.insert(message.clone()) {
                    let _ = self.term.write_line(message);
                }
            }
        }
        for message in &self.update_reference_errors {
            if self.logged.insert(message.clone()) {
                let _ = self.term.write_line(message);
            }
        }
    }

    fn count_all_existing_lines(&self) -> usize {
        let width = self.term.size().1;
        count_lines_per_msg_vec(width, &self.remote_msgs, "remote: ".len())
            + count_lines_per_msg_vec(width, &self.negotiation, 0)
            + count_lines_per_msg_vec(width, &self.transfer_progress_msgs, 0)
            + count_lines_per_msg_vec(width, &self.update_reference_errors, 0)
    }
    fn process_remote_msg(&mut self, data: &[u8]) {
        if let Ok(data) = str::from_utf8(data) {
            let data = data
                .split(['\n', '\r'])
                .map(str::trim)
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<&str>>();
            for data in data {
                let existing_lines = self.count_all_existing_lines();
                let msg = data.to_string();
                if let Some(last) = self.remote_msgs.last() {
                    if (last.contains('%') && !last.contains("100%"))
                        || last == &msg.replace(", done.", "")
                    {
                        self.remote_msgs.pop();
                        self.remote_msgs.push(msg);
                    } else {
                        self.remote_msgs.push(msg);
                    }
                } else {
                    self.remote_msgs.push(msg);
                }
                self.write_all(existing_lines);
            }
        }
    }
    fn process_transfer_progress_update(&mut self, current: usize, total: usize, bytes: usize) {
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }
        if let Some(report) = report_on_transfer_progress(
            current,
            total,
            bytes,
            &self.start_time.unwrap(),
            self.end_time.as_ref(),
        ) {
            let existing_lines = self.count_all_existing_lines();
            if report.contains("100%") {
                self.end_time = Some(Instant::now());
            }
            self.transfer_progress_msgs = vec![report];
            self.write_all(existing_lines);
        }
    }
}

/// Events and exact, operation-scoped publication receipts produced by a
/// successful proposal push. Public compatibility selectors discard receipts;
/// the CLI/internal selector passes them only to its immediate late fanout.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct ProposalPushResult {
    pub events: Vec<Event>,
    pub receipts: Vec<ProposalPublicationReceipt>,
}

#[derive(Clone, Copy)]
enum ProposalStagingDelivery {
    EventFirst,
    LegacyOnly,
}

/// One proposal push destination and its event-publication pairing.
///
/// `relay_origin` is deliberately retained separately from `clone_url`: a
/// GRASP-06 `/prs/<npub>/...` clone URL cannot be normalized back to the relay
/// without accidentally retaining the `/prs` path segment.
#[derive(Clone, Debug)]
struct ProposalServer {
    clone_url: String,
    paired_relay: Option<RelayUrl>,
}

impl ProposalServer {
    /// Preserve the original public helper's push-then-sign behavior. Raw
    /// string URLs have no authenticated client or authoritative relay-origin
    /// context with which to opt into early publication safely.
    fn legacy(clone_url: String) -> Self {
        Self {
            clone_url,
            paired_relay: None,
        }
    }

    fn new(clone_url: String) -> Result<Self> {
        let relay_origin = extract_npub(&clone_url)
            .ok()
            .and_then(|npub| {
                clone_url
                    .find(&format!("/prs/{npub}/"))
                    .map(|position| clone_url[..position].to_string())
            })
            .unwrap_or_else(|| clone_url.clone());
        Self::with_relay_origin(clone_url, &relay_origin)
    }

    fn with_relay_origin(clone_url: String, relay_origin: &str) -> Result<Self> {
        let paired_relay = if is_grasp_server_clone_url(&clone_url) {
            Some(RelayUrl::parse(&format_grasp_server_url_as_relay_url(
                relay_origin,
            )?)?)
        } else {
            None
        };
        Ok(Self {
            clone_url,
            paired_relay,
        })
    }
}

fn add_repo_server_candidate(
    to_try: &mut Vec<ProposalServer>,
    failures: &mut Vec<String>,
    url: &str,
) {
    if to_try.iter().any(|server| server.clone_url == url) {
        return;
    }
    match ProposalServer::new(url.to_string()) {
        Ok(server) => to_try.push(server),
        Err(error) => {
            let reason = format!("{url}: failed to derive its paired GRASP relay: {error:#}");
            eprintln!("warning: skipping repository server {reason}");
            failures.push(reason);
        }
    }
}

fn fallback_proposal_server(
    server: &str,
    public_key: &PublicKey,
    identifier: &str,
) -> Result<ProposalServer> {
    let prs_url = format_grasp_server_url_as_grasp06_prs_url(server, public_key, identifier)
        .with_context(|| format!("invalid fallback GRASP server URL {server}"))?;
    ProposalServer::with_relay_origin(prs_url, server)
        .with_context(|| format!("failed to derive fallback {server}'s paired GRASP relay"))
}

fn record_attempt_failures(failures: &mut Vec<String>, responses: &[(String, Result<()>)]) {
    for (server, response) in responses {
        if let Err(error) = response {
            let reason = format!("{error:#}");
            if reason.contains("early relay publication failed") {
                failures.push(format!(
                    "event-first relay publication failure before Git push to {server}: {reason}"
                ));
            } else if reason.contains("relay publication failed after Git push") {
                failures.push(format!(
                    "relay publication after Git push to {server} failed: {reason}"
                ));
            } else {
                failures.push(format!("Git push to {server} failed: {reason}"));
            }
        }
    }
}

fn selector_failure(message: String, failures: &[String]) -> anyhow::Error {
    if failures.is_empty() {
        anyhow!(message)
    } else {
        anyhow!("{message}\nattempt failures:\n{}", failures.join("\n"))
    }
}

fn repository_is_private(git_repo: &Repo, repo_ref: &RepoRef) -> bool {
    repo_ref.private
        || git_repo
            .git_repo
            .config()
            .ok()
            .and_then(|config| config.get_bool("nostr.private").ok())
            .unwrap_or(false)
}

fn early_staging_permitted(git_repo: &Repo, repo_ref: &RepoRef, paired_relay: &RelayUrl) -> bool {
    staging_relay_is_permitted(
        proposal_repository_only_routing(git_repo, repo_ref),
        &repo_ref.relays,
        paired_relay,
    )
}

fn proposal_repository_only_routing(git_repo: &Repo, repo_ref: &RepoRef) -> bool {
    repository_is_private(git_repo, repo_ref)
        || std::env::var("NGIT_REPO_RELAY_ONLY").is_ok()
        || git_repo
            .git_repo
            .config()
            .ok()
            .and_then(|config| config.get_bool("nostr.repo-relay-only").ok())
            .unwrap_or(false)
}

fn staging_relay_is_permitted(
    repository_restricted: bool,
    configured_relays: &[RelayUrl],
    paired_relay: &RelayUrl,
) -> bool {
    !repository_restricted
        || configured_relays.iter().any(|configured| {
            configured.as_str().trim_end_matches('/') == paired_relay.as_str().trim_end_matches('/')
        })
}

/// Select proposal Git servers and stage eligible signed events before Git,
/// returning the exact early-publication receipts for the immediate fanout.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn select_servers_push_refs_and_generate_pr_or_pr_update_event_with_receipts(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &crate::client::Client,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    proposal_metadata: &ProposalMetadata,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
    git_server: Option<&str>,
) -> Result<ProposalPushResult> {
    select_servers_push_refs_and_generate_pr_or_pr_update_event_with_delivery(
        client,
        git_repo,
        repo_ref,
        tip,
        first_commit,
        merge_base,
        proposal_metadata,
        user_ref,
        root_proposal,
        title_description_overide,
        signer,
        term,
        git_server_push_options,
        git_server,
        ProposalStagingDelivery::EventFirst,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn select_servers_push_refs_and_generate_pr_or_pr_update_event_with_delivery(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &crate::client::Client,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    proposal_metadata: &ProposalMetadata,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
    git_server: Option<&str>,
    staging_delivery: ProposalStagingDelivery,
) -> Result<ProposalPushResult> {
    if let Some(git_server) = git_server {
        remote_helper::validate_git_server_argument(git_server)?;
    }

    let staging_client = match staging_delivery {
        ProposalStagingDelivery::EventFirst => Some(client),
        ProposalStagingDelivery::LegacyOnly => None,
    };

    let mut to_try = vec![];
    let mut tried = vec![];
    let mut selection_failures = vec![];
    let mut ordering_reference = None;
    let repo_grasps = repo_ref.grasp_servers();

    // Determine whether the user-supplied server is a direct clone URL
    // (ends with .git or an SSH URL) or a GRASP server base URL.
    let user_server_is_direct_url = git_server
        .map(|s| {
            s.ends_with(".git") || s.starts_with("git@") || remote_helper::is_direct_pr_clone_url(s)
        })
        .unwrap_or(false);

    // If the user specified a server, put it first in to_try so it is tried
    // before the repo's own servers and its URL appears in the PR clone hint.
    if let Some(server) = git_server {
        if user_server_is_direct_url {
            to_try.push(ProposalServer::new(server.to_string())?);
        } else {
            // GRASP base URL — format as the GRASP-06 /prs/ contributor endpoint.
            let prs_url = format_grasp_server_url_as_grasp06_prs_url(
                server,
                &user_ref.public_key,
                &repo_ref.identifier,
            )?;
            to_try.push(ProposalServer::with_relay_origin(prs_url, server)?);
        }
    }

    if !repo_grasps.is_empty() {
        if !is_quiet() {
            eprintln!(
                "pushing proposal refs to {}",
                if repo_ref.is_authorized_maintainer(&user_ref.public_key) {
                    "repository git servers"
                } else {
                    "repository grasp servers"
                }
            );
        }
    } else if git_server.is_none() {
        eprintln!(
            "The repository doesn't list a grasp server so your proposal cannot be submitted as a nostr Pull Request."
        );
    }

    for url in &repo_ref.git_server {
        match normalize_grasp_server_url(url) {
            Ok(normalized) if repo_grasps.contains(&normalized) => {
                add_repo_server_candidate(&mut to_try, &mut selection_failures, url);
            }
            Err(_) if is_grasp_server_clone_url(url) => {
                // A malformed GRASP-shaped URL cannot appear in grasp_servers;
                // still record its formatting failure before later candidates.
                add_repo_server_candidate(&mut to_try, &mut selection_failures, url);
            }
            _ => {}
        }
    }

    let git_ref: Option<String> = None;

    // --- Primary path: try user-specified server (if any) then repo grasp servers
    // ---
    let (primary_events, primary_responses) =
        push_refs_and_generate_pr_or_pr_update_event_with_client(
            staging_client,
            &mut ordering_reference,
            git_repo,
            repo_ref,
            tip,
            first_commit,
            merge_base,
            proposal_metadata,
            user_ref,
            root_proposal,
            title_description_overide,
            &to_try,
            git_ref.clone(),
            signer,
            term,
            git_server_push_options,
        )
        .await?;
    record_attempt_failures(&mut selection_failures, &primary_responses);
    tried.extend(to_try.iter().map(|server| server.clone_url.clone()));

    let events = if let Some(events) = primary_events {
        events
    } else {
        // GRASP-06 fallback: push to /prs/<contributor-npub>/<identifier>.git on a
        // server from the contributor's KIND_USER_GRASP_LIST (or the default grasp
        // set). No kind-30617 personal-fork announcement is ever created.

        // Build normalised set of servers already tried so we can exclude them.
        let tried_normalized: Vec<String> = tried
            .iter()
            .filter_map(|t| normalize_grasp_server_url(t).ok())
            .collect();

        // 1. Contributor's KIND_USER_GRASP_LIST
        let user_candidates: Vec<String> = user_ref
            .grasp_list
            .urls
            .iter()
            .map(|u| u.to_string())
            .filter(|g| {
                !is_grasp_server_in_list(
                    &normalize_grasp_server_url(g).unwrap_or_default(),
                    &tried_normalized,
                )
            })
            .collect();

        // 2. Default grasp set — only servers not already covered by user_candidates
        let user_candidates_normalized: Vec<String> = user_candidates
            .iter()
            .filter_map(|u| normalize_grasp_server_url(u).ok())
            .collect();
        let default_candidates: Vec<String> = client
            .get_grasp_default_set()
            .iter()
            .filter(|g| {
                let g_norm = normalize_grasp_server_url(g).unwrap_or_default();
                !is_grasp_server_in_list(&g_norm, &tried_normalized)
                    && !is_grasp_server_in_list(&g_norm, &user_candidates_normalized)
            })
            .cloned()
            .collect();

        let all_candidates: Vec<String> = [user_candidates, default_candidates].concat();

        let git_server_hint = match git_server {
            None => "\nspecify a git server with --git-server <url> (or -o git-server=<url> via git push)".to_string(),
            Some(s) => format!("\nthe server you specified ({s}) was tried but failed or was unreachable"),
        };

        if all_candidates.is_empty() {
            if repo_grasps.is_empty() {
                return Err(selector_failure(
                    "failed to push PR: the repository has no grasp servers configured and your \
                     user server list is empty. Add a server to your profile to enable \
                     pushing PRs."
                        .to_string()
                        + &git_server_hint,
                    &selection_failures,
                ));
            }
            return Err(selector_failure(
                "failed to push PR: the repository's grasp servers are down or not accepting \
                 the push right now, and your user server list is empty or all listed \
                 servers were also unreachable. Add a server to your profile and try \
                 again."
                    .to_string()
                    + &git_server_hint,
                &selection_failures,
            ));
        }

        eprintln!(
            "repository servers are down or not accepting your PR right now, pushing to servers in your user server list instead"
        );

        let mut fallback_servers = Vec::new();
        for server in all_candidates {
            match fallback_proposal_server(&server, &user_ref.public_key, &repo_ref.identifier) {
                Ok(server) => fallback_servers.push(server),
                Err(error) => {
                    let reason = format!("{server}: {error:#}");
                    eprintln!("warning: skipping malformed fallback server {reason}");
                    selection_failures.push(reason);
                }
            }
        }
        let mut fallback_events: Option<ProposalPushResult> = None;
        // Keep the established first-success GRASP-06 fallback policy.
        for candidates in fallback_servers.chunks(1) {
            let (events, attempt_responses) =
                push_refs_and_generate_pr_or_pr_update_event_with_client(
                    staging_client,
                    &mut ordering_reference,
                    git_repo,
                    repo_ref,
                    tip,
                    first_commit,
                    merge_base,
                    proposal_metadata,
                    user_ref,
                    root_proposal,
                    title_description_overide,
                    candidates,
                    git_ref.clone(),
                    signer,
                    term,
                    git_server_push_options,
                )
                .await?;
            record_attempt_failures(&mut selection_failures, &attempt_responses);
            if let Some(events) = events {
                fallback_events = Some(events);
                break;
            }
            tried.extend(candidates.iter().map(|server| server.clone_url.clone()));
        }

        fallback_events.ok_or_else(|| {
            selector_failure(
                "failed to push PR: repository servers are down or not accepting the push, \
                 and all fallback servers in your user server list were also unreachable \
                 or rejected the push."
                    .to_string()
                    + &git_server_hint,
                &selection_failures,
            )
        })?
    };

    if staging_client.is_some() && repo_ref.is_authorized_maintainer(&user_ref.public_key) {
        let event = events
            .events
            .first()
            .context("proposal push returned no event")?;
        let refspec = format!("{tip}:refs/nostr/{}", event.id);
        for clone_url in repo_ref
            .git_server
            .iter()
            .filter(|url| !is_grasp_server_clone_url(url) && git_server != Some(url.as_str()))
        {
            if repo_ref.private {
                prepare_private_git_auth(std::slice::from_ref(clone_url), signer).await?;
            }
            if let Err(error) = push_proposal_ref(
                git_repo,
                repo_ref,
                clone_url,
                &refspec,
                term,
                git_server_push_options,
            )
            .await
            {
                term.write_line(&format!(
                    "push: failed to replicate proposal to {}: {error:#}",
                    git_server_display_url(clone_url)
                ))?;
            }
        }
    }
    Ok(events)
}

/// Select proposal Git servers with the original public push-then-sign
/// behavior. This receipt-free API deliberately creates no early relay
/// transports. The internal receipt-carrying selector opts CLI operations into
/// event-first delivery and scopes its evidence to the immediate fanout.
#[allow(clippy::too_many_arguments)]
pub async fn select_servers_push_refs_and_generate_pr_or_pr_update_event(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &crate::client::Client,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    proposal_metadata: &ProposalMetadata,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
    git_server: Option<&str>,
) -> Result<Vec<Event>> {
    Ok(
        select_servers_push_refs_and_generate_pr_or_pr_update_event_with_delivery(
            client,
            git_repo,
            repo_ref,
            tip,
            first_commit,
            merge_base,
            proposal_metadata,
            user_ref,
            root_proposal,
            title_description_overide,
            signer,
            term,
            git_server_push_options,
            git_server,
            ProposalStagingDelivery::LegacyOnly,
        )
        .await?
        .events,
    )
}

/// Compatibility entry point preserving the original public API and its
/// push-then-sign behavior. Callers with raw string URLs do not supply a
/// publication client or trusted GRASP relay-origin context, so this wrapper
/// deliberately does not opt them into event-first staging. The receipt-aware
/// selector carries trusted pairing context explicitly and uses the same core.
#[allow(clippy::too_many_arguments)]
pub async fn push_refs_and_generate_pr_or_pr_update_event(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    proposal_metadata: &ProposalMetadata,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    servers: &[String],
    git_ref: Option<String>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
) -> Result<(Option<Vec<Event>>, Vec<(String, Result<()>)>)> {
    let servers = servers
        .iter()
        .map(|url| ProposalServer::legacy(url.clone()))
        .collect::<Vec<_>>();
    push_refs_and_generate_pr_or_pr_update_event_with_client(
        None,
        &mut None,
        git_repo,
        repo_ref,
        tip,
        first_commit,
        merge_base,
        proposal_metadata,
        user_ref,
        root_proposal,
        title_description_overide,
        &servers,
        git_ref,
        signer,
        term,
        git_server_push_options,
    )
    .await
    .map(|(result, responses)| (result.map(|proposal| proposal.events), responses))
}

#[allow(clippy::too_many_arguments)]
async fn push_refs_and_generate_pr_or_pr_update_event_with_client(
    #[cfg(test)] client: Option<&crate::client::MockConnect>,
    #[cfg(not(test))] client: Option<&crate::client::Client>,
    ordering_reference: &mut Option<Event>,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    proposal_metadata: &ProposalMetadata,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    servers: &[ProposalServer],
    git_ref: Option<String>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
) -> Result<(Option<ProposalPushResult>, Vec<(String, Result<()>)>)> {
    if let Some(client) = client {
        return push_proposal_in_phases(
            client,
            ordering_reference,
            git_repo,
            repo_ref,
            tip,
            first_commit,
            merge_base,
            proposal_metadata,
            user_ref,
            root_proposal,
            title_description_overide,
            servers,
            git_ref,
            signer,
            term,
            git_server_push_options,
        )
        .await;
    }
    let mut responses: Vec<(String, Result<()>)> = vec![];
    let mut unsigned_pr_event: Option<UnsignedEvent> = None;
    for server in servers {
        let clone_url = &server.clone_url;
        let display_url = git_server_display_url(clone_url);
        let mut draft_pr_event = if let Some(ref unsigned_pr_event) = unsigned_pr_event {
            unsigned_pr_event.clone()
        } else {
            let ordering_reference = if let Some(root_proposal) = root_proposal {
                get_all_proposal_patch_pr_pr_update_events_from_cache(
                    git_repo.get_path()?,
                    repo_ref,
                    &root_proposal.id,
                )
                .await
                .ok()
                .and_then(|events| crate::event_ordering::latest_event(&events).cloned())
            } else {
                None
            };
            generate_unsigned_pr_or_update_event(
                git_repo,
                repo_ref,
                &user_ref.public_key,
                root_proposal,
                title_description_overide,
                tip,
                first_commit,
                merge_base,
                proposal_metadata,
                &[clone_url],
                &[],
                git_repo.get_path().ok(),
                ordering_reference.as_ref(),
            )
            .await?
        };

        let git_ref_used = git_ref
            .clone()
            .unwrap_or("refs/nostr/<event-id>".to_string())
            .replace("<event-id>", &draft_pr_event.id().to_string());

        let refspec = format!("{tip}:{git_ref_used}");

        if repo_ref.private {
            prepare_private_git_auth(std::slice::from_ref(clone_url), signer).await?;
        }
        let res = push_proposal_ref(
            git_repo,
            repo_ref,
            clone_url,
            &refspec,
            term,
            git_server_push_options,
        )
        .await;

        match res {
            Err(error) => {
                term.write_line(&format!(
                    "push: error sending commit data to {display_url}: {error}"
                ))?;
                responses.push((clone_url.clone(), Err(anyhow!(error))));
            }
            Ok(()) => {
                responses.push((clone_url.clone(), Ok(())));
                write_progress_line(term, &format!("push: commit data sent to {display_url}"))?;
                unsigned_pr_event = Some(draft_pr_event);
            }
        }
    }
    if let Some(unsigned_pr_event) = unsigned_pr_event {
        let pr_event = sign_proposal_event(unsigned_pr_event, signer, root_proposal).await?;
        let mut result = ProposalPushResult {
            events: vec![pr_event],
            receipts: vec![],
        };
        if root_proposal.is_some_and(|proposal| proposal.kind.eq(&Kind::GitPatch)) {
            result.events.push(
                create_close_status_for_original_patch(
                    git_repo,
                    signer,
                    repo_ref,
                    root_proposal.unwrap(),
                )
                .await?,
            );
        }
        Ok((Some(result), responses))
    } else {
        Ok((None, responses))
    }
}

async fn push_proposal_ref(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    clone_url: &str,
    refspec: &str,
    term: &Term,
    git_server_push_options: &[&str],
) -> Result<()> {
    let result = if is_grasp_server_clone_url(clone_url) {
        push_to_remote_url(
            git_repo,
            clone_url,
            None,
            &[refspec.to_string()],
            term,
            git_server_push_options,
        )
    } else {
        // anticipated only when pushing to user's own repo or a personal-fork with
        // non-grasp git servers. this is used to extract prefered protocols / ssh
        // details from nostr url
        let decoded_nostr_url = {
            if let Ok(Some((_, decoded_nostr_url))) = git_repo
                .get_first_nostr_remote_when_in_ngit_binary()
                .await.context("failed to list git remotes")
                .context("no `nostr://` remote detected. `ngit sync` must be run from a repo with a nostr remote") {
                    decoded_nostr_url
                } else {
                    repo_ref.to_nostr_git_url(&Some(git_repo))
                }
        };
        push_to_remote(
            git_repo,
            clone_url,
            &decoded_nostr_url,
            &[refspec.to_string()],
            term,
            false,
            git_server_push_options,
        )
    };

    let updates = result?;
    for (_, error) in updates {
        if let Some(error) = error {
            return Err(anyhow!(error));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn push_proposal_in_phases(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &crate::client::Client,
    ordering_reference: &mut Option<Event>,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    proposal_metadata: &ProposalMetadata,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    servers: &[ProposalServer],
    git_ref: Option<String>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
) -> Result<(Option<ProposalPushResult>, Vec<(String, Result<()>)>)> {
    if servers.is_empty() {
        return Ok((None, vec![]));
    }
    if ordering_reference.is_none() {
        if let Some(root) = root_proposal {
            let events = get_all_proposal_patch_pr_pr_update_events_from_cache(
                git_repo.get_path()?,
                repo_ref,
                &root.id,
            )
            .await?;
            *ordering_reference = crate::event_ordering::latest_event(&events).cloned();
        }
    }
    // Establish one usable clone hint before replication. A rejected relay or
    // failed Git push cannot remain the advertised location: try the next
    // candidate with a newly signed event containing only its clone URL.
    //
    // Preferred future direction: with multiple repository GRASP servers, omit
    // the explicit clone hint and race each server's EVENT -> Git operation.
    // The first complete success could unlock fanout without waiting for a
    // slow hint server; only exhaustion of repository servers would trigger
    // GRASP-06 fallback. That needs parallel Git transfers and verified
    // no-hint discovery semantics, so keep selection and Git serial here.
    let mut responses = vec![];
    let mut result = ProposalPushResult::default();
    let mut anchor = None;
    for (index, server) in servers.iter().enumerate() {
        let mut draft = generate_unsigned_pr_or_update_event(
            git_repo,
            repo_ref,
            &user_ref.public_key,
            root_proposal,
            title_description_overide,
            tip,
            first_commit,
            merge_base,
            proposal_metadata,
            &[server.clone_url.as_str()],
            &[],
            git_repo.get_path().ok(),
            ordering_reference.as_ref(),
        )
        .await?;
        // Event IDs depend on the unsigned payload, not its signature. The
        // Git-first path can establish this exact ref without asking the signer.
        let draft_id = draft.id();
        let paired_relay = server
            .paired_relay
            .as_ref()
            .filter(|relay| early_staging_permitted(git_repo, repo_ref, relay));
        // Purgatory is required by GRASP. This is a temporary exception for
        // cached Pyramid implementations, which expose unserved PRs immediately.
        // Never block on NIP-11 here: first encounters retain event-first order.
        let git_first =
            paired_relay.is_some_and(crate::relay_information::cached_hint_needs_git_first);
        let mut receipt = None;
        let signed_event = if git_first {
            None
        } else {
            let event = sign_proposal_event(draft.clone(), signer, root_proposal).await?;
            // A failed or timed-out publication may still be visible. Keep
            // subsequent candidates (including GRASP-06 fallback) newer.
            *ordering_reference = Some(event.clone());
            Some(event)
        };
        if let Some(event) = &signed_event {
            if let Some(relay) = paired_relay {
                match stage_signed_proposal_event_for_client(
                    client,
                    event,
                    relay,
                    repository_is_private(git_repo, repo_ref),
                    signer,
                )
                .await
                {
                    Ok(outcome) => {
                        receipt = Some(ProposalPublicationReceipt {
                            event: event.clone(),
                            relay: relay.clone(),
                            outcome,
                        })
                    }
                    Err(EarlyPublicationError::Signing(error)) => return Err(error),
                    Err(EarlyPublicationError::Publication(error)) => {
                        term.write_line(&format!(
                            "push: hint relay admission failed for {}: {error:#}",
                            server.clone_url
                        ))?;
                        responses.push((
                            server.clone_url.clone(),
                            Err(error.context("early relay publication failed")),
                        ));
                        continue;
                    }
                }
            }
        }
        let refspec = format!(
            "{tip}:{}",
            git_ref
                .as_deref()
                .unwrap_or("refs/nostr/<event-id>")
                .replace("<event-id>", &draft_id.to_string())
        );
        if repo_ref.private {
            prepare_private_git_auth(std::slice::from_ref(&server.clone_url), signer).await?;
        }
        match push_proposal_ref(
            git_repo,
            repo_ref,
            &server.clone_url,
            &refspec,
            term,
            git_server_push_options,
        )
        .await
        {
            Ok(()) => {
                write_progress_line(
                    term,
                    &format!(
                        "push: commit data sent to {}",
                        git_server_display_url(&server.clone_url)
                    ),
                )?;
                let event = match signed_event {
                    Some(event) => event,
                    None => {
                        let event = sign_proposal_event(draft, signer, root_proposal).await?;
                        *ordering_reference = Some(event.clone());
                        event
                    }
                };
                if git_first {
                    // Only publish this candidate once its advertised Git data
                    // is usable; a rejected Git push leaves no visible PR.
                    let relay = paired_relay.expect("Git-first requires a paired relay");
                    match stage_signed_proposal_event_for_client(
                        client,
                        &event,
                        relay,
                        repository_is_private(git_repo, repo_ref),
                        signer,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            receipt = Some(ProposalPublicationReceipt {
                                event: event.clone(),
                                relay: relay.clone(),
                                outcome,
                            })
                        }
                        Err(EarlyPublicationError::Signing(error)) => return Err(error),
                        Err(EarlyPublicationError::Publication(error)) => {
                            term.write_line(&format!(
                                "push: hint relay publication failed after Git push to {}: {error:#}",
                                server.clone_url
                            ))?;
                            responses.push((
                                server.clone_url.clone(),
                                Err(error.context("relay publication failed after Git push")),
                            ));
                            continue;
                        }
                    }
                }
                responses.push((server.clone_url.clone(), Ok(())));
                save_event_in_local_cache(git_repo.get_path()?, &event)
                    .await
                    .context("failed to cache the Git-accepted hint proposal event")?;
                result.receipts.extend(receipt);
                result.events.push(event.clone());
                anchor = Some((index, event, refspec));
                break;
            }
            Err(error) => {
                term.write_line(&format!(
                    "push: error sending commit data to {}: {error:#}",
                    server.clone_url
                ))?;
                responses.push((server.clone_url.clone(), Err(error)));
            }
        }
    }
    let Some((anchor_index, event, refspec)) = anchor else {
        return Ok((None, responses));
    };
    let remaining = &servers[anchor_index + 1..];

    // Second stage: publish the confirmed hint's exact event to all remaining
    // relays at once, then perform the remaining serial Git transfers. Ordinary
    // relays must not wait for replica Git pushes or another relay's admission.
    // GRASP rejection affects only that replica; the advertised hint works.
    // Never use the shared relay pool here: its reconnects must not publish a
    // rejected or abandoned proposal after the operation has moved on.
    let mut outcomes: HashMap<_, _> = result
        .receipts
        .iter()
        .map(|receipt| (receipt.relay.clone(), receipt.outcome.clone()))
        .collect();
    let mut relays = Vec::new();
    for server in remaining {
        if let Some(relay) = server
            .paired_relay
            .as_ref()
            .filter(|relay| early_staging_permitted(git_repo, repo_ref, relay))
        {
            if !relays.contains(relay) && !outcomes.contains_key(relay) {
                relays.push(relay.clone());
            }
        }
    }
    let replica_relays = relays.iter().cloned().collect::<HashSet<_>>();
    // Match ordinary publication's repository/user/default target selection,
    // including repository-only and private routing. These targets join the
    // GRASP admissions in this same fanout, not a third post-Git phase.
    let user_relays = user_ref.relays.write();
    let targets = publication_targets(
        &repo_ref.relays,
        &user_relays,
        client.get_relay_default_set(),
        &[],
        proposal_repository_only_routing(git_repo, repo_ref),
        false,
    );
    for relay in targets
        .relays
        .iter()
        .filter_map(|url| RelayUrl::parse(url).ok())
    {
        if !outcomes.contains_key(&relay) && !relays.contains(&relay) {
            relays.push(relay);
        }
    }
    if !relays.is_empty() {
        write_progress_line(
            term,
            "posting proposal to remaining Nostr relays before replicating Git",
        )?;
    }
    let admissions = futures::future::join_all(relays.iter().map(|relay| async {
        stage_signed_proposal_event_for_client(
            client,
            &event,
            relay,
            repository_is_private(git_repo, repo_ref),
            signer,
        )
        .await
    }))
    .await;
    for (relay, admission) in relays.into_iter().zip(admissions) {
        let outcome = match admission {
            Ok(outcome) => outcome,
            Err(EarlyPublicationError::Signing(error)) => return Err(error),
            Err(EarlyPublicationError::Publication(error)) => {
                crate::client::RelayPublishOutcome::rejected(relay.as_str(), format!("{error:#}"))
            }
        };
        if !replica_relays.contains(&relay) {
            result.receipts.push(ProposalPublicationReceipt {
                event: event.clone(),
                relay: relay.clone(),
                outcome: outcome.clone(),
            });
        }
        outcomes.insert(relay, outcome);
    }
    let mut admitted = vec![];
    for server in remaining {
        let receipt = server.paired_relay.as_ref().and_then(|relay| {
            outcomes
                .get(relay)
                .map(|outcome| ProposalPublicationReceipt {
                    event: event.clone(),
                    relay: relay.clone(),
                    outcome: outcome.clone(),
                })
        });
        if let Some(reason) = receipt
            .as_ref()
            .and_then(|receipt| receipt.outcome.error.as_ref())
        {
            term.write_line(&format!(
                "push: admission failed for {}: {reason}",
                server.clone_url
            ))?;
            responses.push((
                server.clone_url.clone(),
                Err(anyhow!("early relay publication failed: {reason}")),
            ));
            result.receipts.extend(receipt);
        } else {
            admitted.push((server, receipt));
        }
    }
    // Git destinations retain their original ordering. The selector then
    // replicates to ordinary Git servers. Exact fanout outcomes prevent the
    // callers from sending this PR event again after the Git transfers.
    for (server, receipt) in admitted {
        if repo_ref.private {
            prepare_private_git_auth(std::slice::from_ref(&server.clone_url), signer).await?;
        }
        let response = push_proposal_ref(
            git_repo,
            repo_ref,
            &server.clone_url,
            &refspec,
            term,
            git_server_push_options,
        )
        .await;
        match &response {
            Ok(()) => {
                write_progress_line(
                    term,
                    &format!(
                        "push: commit data sent to {}",
                        git_server_display_url(&server.clone_url)
                    ),
                )?;
            }
            Err(error) => {
                term.write_line(&format!(
                    "push: error sending commit data to {}: {error:#}",
                    server.clone_url
                ))?;
            }
        }
        // Relay acceptance remains valid: this event advertises the working
        // hint. A replica Git failure must not suppress later close/status events.
        result.receipts.extend(receipt);
        responses.push((server.clone_url.clone(), response));
    }
    if let Some(root) = root_proposal.filter(|root| root.kind == Kind::GitPatch) {
        result
            .events
            .push(create_close_status_for_original_patch(git_repo, signer, repo_ref, root).await?);
    }
    Ok((Some(result), responses))
}

async fn sign_proposal_event(
    draft: UnsignedEvent,
    signer: &Arc<crate::NgitSigner>,
    root_proposal: Option<&Event>,
) -> Result<Event> {
    sign_draft_event(
        draft,
        signer,
        if root_proposal.is_some_and(|proposal| proposal.kind.eq(&Kind::GitPatch)) {
            "Pull Request Replacing Original Patch"
        } else if root_proposal.is_some() {
            "Pull Request Update"
        } else {
            "Pull Request"
        }
        .to_string(),
    )
    .await
}

#[cfg(not(test))]
async fn stage_signed_proposal_event_for_client(
    client: &crate::client::Client,
    event: &Event,
    paired_relay: &RelayUrl,
    private_repository: bool,
    signer: &Arc<crate::NgitSigner>,
) -> Result<crate::client::RelayPublishOutcome, EarlyPublicationError> {
    client
        .stage_signed_proposal_event(event, paired_relay, private_repository, signer)
        .await
}

#[cfg(test)]
async fn stage_signed_proposal_event_for_client(
    _client: &crate::client::MockConnect,
    event: &Event,
    paired_relay: &RelayUrl,
    private_repository: bool,
    signer: &Arc<crate::NgitSigner>,
) -> Result<crate::client::RelayPublishOutcome, EarlyPublicationError> {
    // Unit tests do not route staging through the mock's shared send_event_to
    // method. Use the same production direct-Relay port; integration binaries
    // additionally exercise the configured proactive-auth flag.
    crate::client::Client::default()
        .stage_signed_proposal_event(event, paired_relay, private_repository, signer)
        .await
}

fn git_server_display_url(url: &str) -> String {
    if remote_helper::handles_url(url) {
        url.to_string()
    } else {
        normalize_grasp_server_url(url).unwrap_or_else(|_| get_short_git_server_name(url))
    }
}

async fn create_close_status_for_original_patch(
    git_repo: &Repo,
    signer: &Arc<crate::NgitSigner>,
    repo_ref: &RepoRef,
    proposal: &Event,
) -> Result<Event> {
    let mut public_keys = repo_ref
        .maintainers
        .iter()
        .copied()
        .collect::<HashSet<PublicKey>>();
    public_keys.insert(proposal.pubkey);

    let statuses = crate::client::get_events_from_local_cache(
        git_repo.get_path()?,
        vec![
            nostr::prelude::Filter::new()
                .kinds(crate::git_events::status_kinds())
                .event(proposal.id),
        ],
    )
    .await?;
    let statuses: Vec<_> = statuses
        .into_iter()
        .filter(|event| {
            event.pubkey == proposal.pubkey || repo_ref.is_authorized_member(&event.pubkey)
        })
        .collect();
    crate::git_events::sign_ordered_status_event(
        EventBuilder::new(nostr::event::Kind::GitStatusClosed, String::new()).tags(
            [
                vec![
                    Tag::parse(["alt", "Git patch closed as forthcoming update is too large. Replacing with Pull Request"]).unwrap(),
                    Tag::from(nostr::nips::nip01::Nip01Tag::Event {
                        id: proposal.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        public_key: None,
                    }),
                ],
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        Tag::from(nostr::nips::nip01::Nip01Tag::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_hint: c.relays.first().cloned(),
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![
                    Tag::parse(["r", &repo_ref.root_commit.to_string()]).unwrap(),
                ],
            ]
            .concat(),
        ),
        signer,
        &statuses,
        proposal,
        repo_ref,
        "close status for original patch".to_string(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        ProposalServer, fallback_proposal_server, git_server_display_url,
        staging_relay_is_permitted,
    };

    #[test]
    fn transfer_logs_preserve_failures_without_repeating_byte_updates() {
        let term = console::Term::buffered_stderr();
        let mut reporter = super::PushReporter::new(&term);
        reporter.policy = crate::output_mode::ProgressPolicy::new(
            crate::output_mode::OutputMode::Normal,
            true,
            true,
        );
        reporter.update_reference_errors.push("rejected ref".into());
        for current in [1, 2, 2] {
            reporter.process_transfer_progress_update(current, 2, 100);
        }
        assert_eq!(reporter.logged.len(), 1);
        assert!(reporter.logged.contains("rejected ref"));
        reporter.policy.detailed = true;
        reporter.process_transfer_progress_update(2, 2, 100);
        reporter.process_transfer_progress_update(2, 2, 100);
        assert_eq!(reporter.logged.len(), 2);
    }

    #[test]
    fn restricted_staging_requires_an_authoritative_relay_match() {
        let declared =
            super::RelayUrl::parse("ws://example.com/base-path/").expect("valid declared relay");
        let paired =
            super::RelayUrl::parse("ws://example.com/base-path").expect("valid paired relay");
        let undeclared =
            super::RelayUrl::parse("ws://elsewhere.invalid").expect("valid undeclared relay");
        assert!(staging_relay_is_permitted(
            true,
            std::slice::from_ref(&declared),
            &paired
        ));
        assert!(!staging_relay_is_permitted(
            true,
            std::slice::from_ref(&declared),
            &undeclared
        ));
        assert!(!staging_relay_is_permitted(true, &[], &undeclared));
        assert!(staging_relay_is_permitted(false, &[], &undeclared));
    }

    #[test]
    fn malformed_fallback_is_an_isolated_candidate_error() {
        let public_key = nostr::prelude::Keys::generate().public_key();
        let error = fallback_proposal_server("", &public_key, "repo")
            .expect_err("an empty fallback server cannot be normalized");
        assert!(format!("{error:#}").contains("invalid fallback GRASP server URL"));

        let valid = fallback_proposal_server("https://example.com", &public_key, "repo")
            .expect("the next valid fallback must remain usable");
        assert!(valid.clone_url.starts_with("https://example.com/prs/"));
        assert_eq!(
            valid.paired_relay.expect("valid GRASP pair").as_str(),
            "wss://example.com"
        );
    }

    #[test]
    fn proposal_server_derives_relay_from_the_origin_before_prs() {
        const NPUB: &str = "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr";
        for (clone_url, expected_relay) in [
            (
                format!("https://example.com/services/grasp/prs/{NPUB}/repo.git"),
                "wss://example.com/services/grasp",
            ),
            (
                format!("http://example.com/prs/{NPUB}/repo.git"),
                "ws://example.com",
            ),
            (
                format!("https://example.com/services/grasp/{NPUB}/repo.git"),
                "wss://example.com/services/grasp",
            ),
        ] {
            let server = ProposalServer::new(clone_url.clone())
                .unwrap_or_else(|error| panic!("failed to pair {clone_url}: {error}"));
            assert_eq!(
                server
                    .paired_relay
                    .expect("GRASP server must have a paired relay")
                    .as_str()
                    .trim_end_matches('/'),
                expected_relay
            );
        }
    }

    #[test]
    fn helper_urls_do_not_require_grasp_normalization() {
        for url in [
            "htree://npub1example/project",
            "ext::%S /tmp/project.git",
            "file:///tmp/project.git",
        ] {
            assert_eq!(git_server_display_url(url), url);
        }
    }

    mod system_git_fallback {
        use std::{
            io::{Read, Write},
            net::{TcpListener, TcpStream},
            path::Path,
            process::{Child, Command, Stdio},
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            thread,
            time::{Duration, Instant},
        };

        use anyhow::{Context, Result, ensure};
        use console::Term;
        use nostr::prelude::{Kind, PublicKey, nip01::Coordinate, nip19::Nip19Coordinate};

        use crate::{
            git::{
                Repo,
                nostr_url::{NostrUrlDecoded, ServerProtocol},
            },
            push::push_to_remote,
        };

        const CHILD_TEST: &str = "NGIT_FALLBACK_FIXTURE_CHILD";
        const TEST_NAME: &str = "push::tests::system_git_fallback::falls_back_to_system_git_when_libgit2_hits_an_internal_assertion";

        // Isolate the synchronous libgit2 call too: an async timeout cannot
        // cancel it. The child owns a separate process group on Unix, so the
        // deadline also terminates receive-pack and fallback Git processes.
        fn isolated_test(name: &str, deadline: Duration) -> Result<()> {
            let log = tempfile::tempfile()?;
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args(["--exact", name, "--nocapture"])
                .env(CHILD_TEST, name)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log.try_clone()?));
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let mut child = ReapChild(command.spawn()?);
            #[cfg(unix)]
            let mut group = ProcessGroupGuard(Some(child.0.id()));
            let end = Instant::now() + deadline;
            let status = loop {
                if let Some(status) = child.0.try_wait()? {
                    #[cfg(unix)]
                    {
                        group.0 = None;
                    }
                    break Some(status);
                }
                if Instant::now() >= end {
                    #[cfg(unix)]
                    // SAFETY: the unreaped child leads the group we created.
                    unsafe {
                        libc::kill(-(child.0.id() as libc::pid_t), libc::SIGKILL);
                    }
                    let _ = child.0.kill();
                    let _ = child.0.wait();
                    #[cfg(unix)]
                    {
                        group.0 = None;
                    }
                    break None;
                }
                thread::sleep(Duration::from_millis(20));
            };
            use std::io::{Seek, SeekFrom};
            let mut log = log;
            log.seek(SeekFrom::Start(
                log.metadata()?.len().saturating_sub(64 * 1024),
            ))?;
            let mut output = Vec::new();
            log.take(64 * 1024).read_to_end(&mut output)?;
            ensure!(
                status.is_some_and(|status| status.success()),
                "isolated test {name} failed (status: {status:?}, deadline: {deadline:?}):\n{}",
                String::from_utf8_lossy(&output)
            );
            Ok(())
        }

        // Also cover errors while polling: kill descendants before reaping
        // the group leader, whose PID must not yet be reusable.
        #[cfg(unix)]
        struct ProcessGroupGuard(Option<u32>);
        #[cfg(unix)]
        impl Drop for ProcessGroupGuard {
            fn drop(&mut self) {
                if let Some(id) = self.0 {
                    // SAFETY: this is our still-unreaped child's own group.
                    unsafe {
                        libc::kill(-(id as libc::pid_t), libc::SIGKILL);
                    }
                }
            }
        }

        struct ReapChild(Child);
        impl Drop for ReapChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        struct Proxy {
            address: std::net::SocketAddr,
            stopped: Arc<AtomicBool>,
            connections: Arc<AtomicUsize>,
            sockets: Arc<Mutex<Vec<TcpStream>>>,
            task: Option<thread::JoinHandle<()>>,
        }

        impl Proxy {
            fn start(repo: &Path) -> Result<Self> {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                let address = listener.local_addr()?;
                let stopped = Arc::new(AtomicBool::new(false));
                let connections = Arc::new(AtomicUsize::new(0));
                let sockets = Arc::new(Mutex::new(Vec::new()));
                let task = thread::spawn({
                    let stopped = stopped.clone();
                    let connections = connections.clone();
                    let sockets = sockets.clone();
                    let repo = repo.to_path_buf();
                    move || {
                        let mut workers = Vec::new();
                        for stream in listener.incoming() {
                            if stopped.load(Ordering::SeqCst) {
                                break;
                            }
                            let Ok(stream) = stream else {
                                break;
                            };
                            connections.fetch_add(1, Ordering::SeqCst);
                            match stream.try_clone() {
                                Ok(socket) => sockets.lock().unwrap().push(socket),
                                Err(error) => {
                                    eprintln!("retain proxy socket: {error}");
                                    break;
                                }
                            }
                            let repo = repo.clone();
                            // The failed libgit2 session must not prevent the
                            // fallback client from receiving its advertisement.
                            workers.push(thread::spawn(move || {
                                if let Err(error) = serve_connection(stream, &repo) {
                                    eprintln!("fallback proxy connection: {error:#}");
                                }
                            }));
                        }
                        for socket in sockets.lock().unwrap().iter() {
                            let _ = socket.shutdown(std::net::Shutdown::Both);
                        }
                        for worker in workers {
                            let _ = worker.join();
                        }
                    }
                });
                Ok(Self {
                    address,
                    stopped,
                    connections,
                    sockets,
                    task: Some(task),
                })
            }
        }

        impl Drop for Proxy {
            fn drop(&mut self) {
                self.stopped.store(true, Ordering::SeqCst);
                for socket in self.sockets.lock().unwrap().iter() {
                    let _ = socket.shutdown(std::net::Shutdown::Both);
                }
                // Wake accept without releasing/rebinding the fixture's port.
                let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
                if let Some(task) = self.task.take() {
                    let _ = task.join();
                }
            }
        }

        fn run<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
            let mut command = Command::new("git");
            let output = crate::git::detach_from_inherited_repository(&mut command)
                .current_dir(cwd)
                .args(args)
                .output()?;
            ensure!(
                output.status.success(),
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(String::from_utf8(output.stdout)?.trim().to_string())
        }

        fn decoded_nostr_url_forcing_git_protocol() -> NostrUrlDecoded {
            NostrUrlDecoded {
                original_string: String::new(),
                coordinate: Nip19Coordinate {
                    coordinate: Coordinate {
                        identifier: "system-git-fallback-test".to_string(),
                        public_key: PublicKey::parse(
                            "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                        )
                        .expect("valid public key"),
                        kind: Kind::GitRepoAnnouncement,
                    },
                    relays: vec![],
                },
                protocol: Some(ServerProtocol::Git),
                ssh_key_file: None,
                nip05: None,
            }
        }

        fn read_pkt(stream: &mut impl Read) -> Result<Option<Vec<u8>>> {
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header)?;
            let len = usize::from_str_radix(std::str::from_utf8(&header)?, 16)?;
            if len == 0 {
                return Ok(None);
            }
            ensure!(len >= 4, "invalid pkt-line length");
            let mut payload = vec![0_u8; len - 4];
            stream.read_exact(&mut payload)?;
            Ok(Some(payload))
        }

        fn write_pkt(stream: &mut impl Write, payload: &[u8]) -> Result<()> {
            write!(stream, "{:04x}", payload.len() + 4)?;
            stream.write_all(payload)?;
            Ok(())
        }

        /// Reorder the capabilities of the first ref-advertisement pkt so
        /// `object-format=sha1` is the final byte of the pkt, as tangled.org
        /// does. libgit2 1.9 fails `GIT_ASSERT(eos)` (smart_pkt.c `set_data`)
        /// on such advertisements while git parses them fine.
        fn rewrite_first_advertisement_pkt(payload: &[u8]) -> Result<Vec<u8>> {
            let nul = payload
                .iter()
                .position(|b| *b == 0)
                .context("first advertisement pkt has no capabilities")?;
            let mut caps = std::str::from_utf8(&payload[nul + 1..])?
                .split_whitespace()
                .filter(|cap| !cap.starts_with("object-format=") && !cap.starts_with("agent="))
                .collect::<Vec<&str>>();
            caps.push("object-format=sha1");
            let mut rewritten = payload[..=nul].to_vec();
            rewritten.extend_from_slice(caps.join(" ").as_bytes());
            Ok(rewritten)
        }

        /// Copy until EOF or a write failure with plain read/write calls.
        /// `std::io::copy` is avoided because its Linux splice fast path
        /// stalled this pipe-to-socket relay.
        fn relay(reader: &mut impl Read, writer: &mut impl Write) {
            let mut buf = [0_u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        }

        /// Serve one `git://` connection by proxying to `git receive-pack`
        /// on `repo`, rewriting the advertisement to the tangled.org shape.
        fn serve_connection(mut stream: TcpStream, repo: &Path) -> Result<()> {
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            // Discard the git-daemon request line; the repository is fixed.
            read_pkt(&mut stream)?.context("expected a git protocol request pkt")?;

            let mut child = ReapChild(
                Command::new("git")
                    .arg("receive-pack")
                    .arg(repo)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    // The libgit2 connection aborts mid-session by design, which
                    // makes receive-pack print "the remote end hung up".
                    .stderr(Stdio::null())
                    .spawn()?,
            );
            let mut child_in = child.0.stdin.take().context("no receive-pack stdin")?;
            let mut child_out = child.0.stdout.take().context("no receive-pack stdout")?;

            let mut first = true;
            while let Some(payload) = read_pkt(&mut child_out)? {
                let payload = if first {
                    first = false;
                    rewrite_first_advertisement_pkt(&payload)?
                } else {
                    payload
                };
                write_pkt(&mut stream, &payload)?;
            }
            stream.write_all(b"0000")?;
            stream.flush()?;

            // Relay the rest of the session transparently in both directions.
            let mut stream_reader = stream.try_clone()?;
            let to_child = thread::spawn(move || {
                relay(&mut stream_reader, &mut child_in);
            });
            relay(&mut child_out, &mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            let _ = to_child.join();
            let _ = child.0.wait();
            Ok(())
        }

        #[test]
        fn stalled_connection_does_not_block_next_client() -> Result<()> {
            const NAME: &str =
                "push::tests::system_git_fallback::stalled_connection_does_not_block_next_client";
            if std::env::var(CHILD_TEST).as_deref() != Ok(NAME) {
                return isolated_test(NAME, Duration::from_secs(30));
            }
            let repo = tempfile::tempdir()?;
            run(repo.path(), ["init", "-q", "--bare"])?;
            let proxy = Proxy::start(repo.path())?;
            let mut stalled = TcpStream::connect(proxy.address)?;
            stalled.set_read_timeout(Some(Duration::from_secs(5)))?;
            write_pkt(&mut stalled, b"git-receive-pack /remote\0host=localhost\0")?;
            // Observe an accepted, active session before opening the next one.
            while read_pkt(&mut stalled)?.is_some() {}
            let mut next = TcpStream::connect(proxy.address)?;
            next.set_read_timeout(Some(Duration::from_secs(5)))?;
            write_pkt(&mut next, b"git-receive-pack /remote\0host=localhost\0")?;
            ensure!(read_pkt(&mut next)?.is_some(), "missing advertisement");
            // Both sessions remain open while Drop stops and joins the proxy.
            drop(proxy);
            Ok(())
        }

        #[test]
        fn isolated_deadline_terminates_stalled_test() -> Result<()> {
            const NAME: &str =
                "push::tests::system_git_fallback::isolated_deadline_terminates_stalled_test";
            if std::env::var(CHILD_TEST).as_deref() == Ok(NAME) {
                loop {
                    thread::park();
                }
            }
            let error = isolated_test(NAME, Duration::from_secs(1))
                .expect_err("a stalled test must hit its deadline");
            ensure!(error.to_string().contains("status: None"), "{error:#}");
            Ok(())
        }

        #[test]
        fn falls_back_to_system_git_when_libgit2_hits_an_internal_assertion() -> Result<()> {
            if std::env::var(CHILD_TEST).as_deref() != Ok(TEST_NAME) {
                return isolated_test(TEST_NAME, Duration::from_secs(60));
            }
            let temp = tempfile::tempdir()?;
            let source_path = temp.path().join("source");
            let remote_path = temp.path().join("remote.git");

            run(
                temp.path(),
                ["init", "-q", "-b", "main", source_path.to_str().unwrap()],
            )?;
            run(&source_path, ["config", "user.name", "ngit test"])?;
            run(
                &source_path,
                ["config", "user.email", "ngit-test@example.invalid"],
            )?;
            run(
                &source_path,
                ["commit", "-q", "--allow-empty", "-m", "initial"],
            )?;
            let oid = run(&source_path, ["rev-parse", "HEAD"])?;
            run(
                temp.path(),
                ["init", "-q", "--bare", remote_path.to_str().unwrap()],
            )?;

            let proxy = Proxy::start(&remote_path)?;
            let port = proxy.address.port();

            let updates = push_to_remote(
                &Repo::from_path(&source_path)?,
                &format!("git://127.0.0.1:{port}/remote"),
                &decoded_nostr_url_forcing_git_protocol(),
                &["refs/heads/main:refs/heads/main".to_string()],
                &Term::buffered_stderr(),
                false,
                &[],
            )?;

            // `push_to_remote` reports per-ref entries only for rejections.
            assert!(updates.values().all(Option::is_none));
            // The first connection is libgit2 failing on the advertisement;
            // the push must have completed over a second, system-git
            // connection. A single connection means libgit2 no longer
            // asserts and this fallback deserves a rethink.
            assert_eq!(proxy.connections.load(Ordering::SeqCst), 2);
            let pushed = run(
                temp.path(),
                [
                    "--git-dir",
                    remote_path.to_str().unwrap(),
                    "rev-parse",
                    "refs/heads/main",
                ],
            )?;
            assert_eq!(pushed, oid);
            Ok(())
        }
    }
}
