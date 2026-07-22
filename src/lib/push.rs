use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use auth_git2::GitAuthenticator;
use console::Term;
use nostr::{
    event::{Event, EventBuilder, Kind, Tag, TagCodec, UnsignedEvent},
    hashes::sha1::Hash as Sha1Hash,
    key::PublicKey,
};

use crate::{
    cli_interactor::count_lines_per_msg_vec,
    client::{Connect, sign_draft_event, sign_event},
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        oid_to_shorthand_string, remote_helper,
    },
    git_events::{KIND_PULL_REQUEST_UPDATE, generate_unsigned_pr_or_update_event},
    login::user::UserRef,
    repo_ref::{
        RepoRef, format_grasp_server_url_as_grasp06_prs_url, is_grasp_server_clone_url,
        is_grasp_server_in_list, normalize_grasp_server_url,
    },
    utils::{
        Direction, get_short_git_server_name, get_write_protocols_to_try, join_with_and,
        set_protocol_preference,
    },
};

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
    if remote_helper::handles_url(git_server_url) {
        term.write_line(&format!("push: {git_server_url} via Git remote helper..."))?;
        return remote_helper::push(
            git_repo,
            git_server_url,
            remote_refspecs,
            term,
            git_server_push_options,
        );
    }

    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_write_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

    let mut failed_protocols = vec![];
    let mut success = false;
    let mut ref_updates = HashMap::new();

    for protocol in &protocols_to_attempt {
        term.write_line(format!("push: {} over {protocol}...", server_url.short_name(),).as_str())?;

        let formatted_url = server_url.format_as(protocol)?;

        match push_to_remote_url(
            git_repo,
            &formatted_url,
            decoded_nostr_url.ssh_key_file_path().as_ref(),
            remote_refspecs,
            term,
            git_server_push_options,
        ) {
            Err(error) => {
                term.write_line(
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
                        term.write_line(format!("push: succeeded over {protocol}").as_str())?;
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
        term.write_line(format!("push: {error}").as_str())?;
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
            start_time: None,
            end_time: None,
        }
    }
    fn write_all(&self, lines_to_clear: usize) {
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

#[allow(clippy::too_many_arguments)]
pub async fn select_servers_push_refs_and_generate_pr_or_pr_update_event(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &crate::client::Client,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
    git_server: Option<&str>,
) -> Result<Vec<Event>> {
    let mut to_try = vec![];
    let mut tried = vec![];
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
            to_try.push(server.to_string());
        } else {
            // GRASP base URL — format as the GRASP-06 /prs/ contributor endpoint.
            let prs_url = format_grasp_server_url_as_grasp06_prs_url(
                server,
                &user_ref.public_key,
                &repo_ref.identifier,
            )?;
            to_try.push(prs_url);
        }
    }

    if !repo_grasps.is_empty() {
        eprintln!(
            "pushing proposal refs to {}",
            if repo_ref.maintainers.contains(&user_ref.public_key) {
                "repository git servers"
            } else {
                "repository grasp servers"
            }
        );
    } else if git_server.is_none() {
        eprintln!(
            "The repository doesn't list a grasp server so your proposal cannot be submitted as a nostr Pull Request."
        );
    }

    // Append repo GRASP servers after the user-specified server.
    for url in &repo_ref.git_server {
        if let Ok(normalized) = normalize_grasp_server_url(url) {
            if repo_grasps.contains(&normalized) && !to_try.contains(url) {
                to_try.push(url.clone());
            }
        }
    }

    let git_ref: Option<String> = None;

    // --- Primary path: try user-specified server (if any) then repo grasp servers
    // ---
    let (primary_events, _server_responses) = push_refs_and_generate_pr_or_pr_update_event(
        git_repo,
        repo_ref,
        tip,
        first_commit,
        merge_base,
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
    tried.extend(to_try);

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
                bail!(
                    "failed to push PR: the repository has no grasp servers configured and your \
                     user server list is empty. Add a server to your profile to enable \
                     pushing PRs.{git_server_hint}"
                )
            }
            bail!(
                "failed to push PR: the repository's grasp servers are down or not accepting \
                 the push right now, and your user server list is empty or all listed \
                 servers were also unreachable. Add a server to your profile and try \
                 again.{git_server_hint}"
            )
        }

        eprintln!(
            "repository servers are down or not accepting your PR right now, pushing to servers in your user server list instead"
        );

        let mut fallback_events: Option<Vec<Event>> = None;
        for server in all_candidates {
            let prs_url = format_grasp_server_url_as_grasp06_prs_url(
                &server,
                &user_ref.public_key,
                &repo_ref.identifier,
            )?;
            let (events, _) = push_refs_and_generate_pr_or_pr_update_event(
                git_repo,
                repo_ref,
                tip,
                first_commit,
                merge_base,
                user_ref,
                root_proposal,
                title_description_overide,
                std::slice::from_ref(&prs_url),
                git_ref.clone(),
                signer,
                term,
                git_server_push_options,
            )
            .await?;
            if let Some(events) = events {
                fallback_events = Some(events);
                break;
            }
            tried.push(prs_url);
        }

        fallback_events.ok_or_else(|| {
            anyhow::anyhow!(
                "failed to push PR: repository servers are down or not accepting the push, \
                 and all fallback servers in your user server list were also unreachable \
                 or rejected the push.{git_server_hint}"
            )
        })?
    };

    eprintln!(
        "posting {}",
        if events.iter().any(|e| e.kind.eq(&Kind::GitStatusClosed)) {
            "proposal revision as new PR event, and a close status for the old patch"
        } else if events.iter().any(|e| e.kind.eq(&KIND_PULL_REQUEST_UPDATE)) {
            "proposal revision as PR update event"
        } else {
            "proposal as PR event"
        }
    );
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
pub async fn push_refs_and_generate_pr_or_pr_update_event(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    first_commit: &Sha1Hash,
    merge_base: Option<&Sha1Hash>,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    servers: &[String],
    git_ref: Option<String>,
    signer: &Arc<crate::NgitSigner>,
    term: &Term,
    git_server_push_options: &[&str],
) -> Result<(Option<Vec<Event>>, Vec<(String, Result<()>)>)> {
    let mut responses: Vec<(String, Result<()>)> = vec![];

    let mut unsigned_pr_event: Option<UnsignedEvent> = None;
    for clone_url in servers {
        let display_url = git_server_display_url(clone_url);
        let mut draft_pr_event = if let Some(ref unsigned_pr_event) = unsigned_pr_event {
            unsigned_pr_event.clone()
        } else {
            generate_unsigned_pr_or_update_event(
                git_repo,
                repo_ref,
                &user_ref.public_key,
                root_proposal,
                title_description_overide,
                tip,
                first_commit,
                merge_base,
                &[clone_url],
                &[],
                git_repo.get_path().ok(),
            )
            .await?
        };

        let git_ref_used = git_ref
            .clone()
            .unwrap_or("refs/nostr/<event-id>".to_string())
            .replace("<event-id>", &draft_pr_event.id().to_string());

        let refspec = format!("{tip}:{git_ref_used}");

        let res = if is_grasp_server_clone_url(clone_url) {
            push_to_remote_url(
                git_repo,
                clone_url,
                None,
                &[refspec],
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
                &[refspec],
                term,
                false,
                git_server_push_options,
            )
        };

        match res {
            Err(error) => {
                term.write_line(&format!(
                    "push: error sending commit data to {display_url}: {error}"
                ))?;
                responses.push((clone_url.clone(), Err(anyhow!(error))));
            }
            Ok(ref_updates) => {
                if let Some((_, Some(error))) = ref_updates.iter().next() {
                    term.write_line(&format!(
                        "push: error sending commit data to {display_url}: {error}"
                    ))?;
                    responses.push((clone_url.clone(), Err(anyhow!(error.clone()))));
                } else {
                    responses.push((clone_url.clone(), Ok(())));
                    term.write_line(&format!("push: commit data sent to {display_url}"))?;
                    unsigned_pr_event = Some(draft_pr_event);
                }
            }
        }
    }
    if let Some(unsigned_pr_event) = unsigned_pr_event {
        let pr_event = sign_draft_event(
            unsigned_pr_event,
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
        .await?;
        if root_proposal.is_some_and(|proposal| proposal.kind.eq(&Kind::GitPatch)) {
            Ok((
                Some(vec![
                    pr_event,
                    create_close_status_for_original_patch(
                        signer,
                        repo_ref,
                        root_proposal.unwrap(),
                    )
                    .await?,
                ]),
                responses,
            ))
        } else {
            Ok((Some(vec![pr_event]), responses))
        }
    } else {
        Ok((None, responses))
    }
}

fn git_server_display_url(url: &str) -> String {
    if remote_helper::handles_url(url) {
        url.to_string()
    } else {
        normalize_grasp_server_url(url).unwrap_or_else(|_| get_short_git_server_name(url))
    }
}

async fn create_close_status_for_original_patch(
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

    sign_event(
        EventBuilder::new(nostr::event::Kind::GitStatusClosed, String::new()).tags(
            [
                vec![
                    Tag::parse(["alt", "Git patch closed as forthcoming update is too large. Replacing with Pull Request"]).unwrap(),
                    nostr::nips::nip01::Nip01Tag::Event {
                        id: proposal.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        public_key: None,
                    }.to_tag(),
                ],
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        nostr::nips::nip01::Nip01Tag::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_hint: c.relays.first().cloned(),
                        }.to_tag()
                    })
                    .collect::<Vec<Tag>>(),
                vec![
                    Tag::parse(["r", &repo_ref.root_commit.to_string()]).unwrap(),
                ],
            ]
            .concat(),
        ),
        signer,
        "close status for original patch".to_string(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::git_server_display_url;

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
}
