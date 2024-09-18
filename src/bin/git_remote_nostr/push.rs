use core::str;
use std::{
    collections::{HashMap, HashSet},
    io::Stdin,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{anyhow, bail, Context, Result};
use auth_git2::GitAuthenticator;
use client::{get_events_from_cache, get_state_from_cache, send_events, sign_event, STATE_KIND};
use console::Term;
use git::{sha1_to_oid, RepoActions};
use git2::{Oid, Repository};
use git_events::{
    generate_cover_letter_and_patch_events, generate_patch_event, get_commit_id_from_patch,
};
use ngit::{
    client::{self, get_event_from_cache_by_id},
    git::{
        self,
        nostr_url::{CloneUrl, NostrUrlDecoded},
        oid_to_shorthand_string,
    },
    git_events::{self, get_event_root},
    login::{self, get_curent_user},
    repo_ref, repo_state,
};
use nostr::nips::nip10::Marker;
use nostr_sdk::{
    hashes::sha1::Hash as Sha1Hash, Event, EventBuilder, EventId, Kind, PublicKey, Tag,
};
use nostr_signer::NostrSigner;
use repo_ref::RepoRef;
use repo_state::RepoState;

use crate::{
    client::Client,
    git::Repo,
    list::list_from_remotes,
    utils::{
        count_lines_per_msg_vec, find_proposal_and_patches_by_branch_name, get_all_proposals,
        get_remote_name_by_url, get_short_git_server_name, get_write_protocols_to_try,
        join_with_and, push_error_is_not_authentication_failure, read_line,
        set_protocol_preference, Direction,
    },
};

#[allow(clippy::too_many_lines)]
pub async fn run_push(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    decoded_nostr_url: &NostrUrlDecoded,
    stdin: &Stdin,
    initial_refspec: &str,
    client: &Client,
    list_outputs: Option<HashMap<String, HashMap<String, String>>>,
) -> Result<()> {
    let refspecs = get_refspecs_from_push_batch(stdin, initial_refspec)?;

    let proposal_refspecs = refspecs
        .iter()
        .filter(|r| r.contains("refs/heads/pr/"))
        .cloned()
        .collect::<Vec<String>>();

    let mut git_server_refspecs = refspecs
        .iter()
        .filter(|r| !r.contains("refs/heads/pr/"))
        .cloned()
        .collect::<Vec<String>>();

    let term = console::Term::stderr();

    let list_outputs = match list_outputs {
        Some(outputs) => outputs,
        _ => list_from_remotes(&term, git_repo, &repo_ref.git_server, decoded_nostr_url),
    };

    let nostr_state = get_state_from_cache(git_repo.get_path()?, repo_ref).await;

    let existing_state = {
        // if no state events - create from first git server listed
        if let Ok(nostr_state) = &nostr_state {
            nostr_state.state.clone()
        } else if let Some(url) = repo_ref
            .git_server
            .iter()
            .find(|&url| list_outputs.contains_key(url))
        {
            list_outputs.get(url).unwrap().to_owned()
        } else {
            bail!(
                "cannot connect to git servers: {}",
                repo_ref.git_server.join(" ")
            );
        }
    };

    let (rejected_refspecs, remote_refspecs) = create_rejected_refspecs_and_remotes_refspecs(
        &term,
        git_repo,
        &git_server_refspecs,
        &existing_state,
        &list_outputs,
    )?;

    git_server_refspecs.retain(|refspec| {
        if let Some(rejected) = rejected_refspecs.get(&refspec.to_string()) {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            println!("error {to} {} out of sync with nostr", rejected.join(" "));
            false
        } else {
            true
        }
    });

    let mut events = vec![];

    if git_server_refspecs.is_empty() && proposal_refspecs.is_empty() {
        // all refspecs rejected
        println!();
        return Ok(());
    }

    let (signer, user_ref) = login::launch(
        git_repo,
        &None,
        &None,
        &None,
        &None,
        Some(client),
        false,
        true,
    )
    .await?;

    if !repo_ref.maintainers.contains(&user_ref.public_key) {
        for refspec in &git_server_refspecs {
            let (_, to) = refspec_to_from_to(refspec).unwrap();
            println!(
                "error {to} your nostr account {} isn't listed as a maintainer of the repo",
                user_ref.metadata.name
            );
        }
        git_server_refspecs.clear();
        if proposal_refspecs.is_empty() {
            println!();
            return Ok(());
        }
    }

    if !git_server_refspecs.is_empty() {
        let new_state = generate_updated_state(git_repo, &existing_state, &git_server_refspecs)?;

        let new_repo_state =
            RepoState::build(repo_ref.identifier.clone(), new_state, &signer).await?;

        events.push(new_repo_state.event);

        for event in get_merged_status_events(
            &term,
            repo_ref,
            git_repo,
            &decoded_nostr_url.original_string,
            &signer,
            &git_server_refspecs,
        )
        .await?
        {
            events.push(event);
        }
    }

    let mut rejected_proposal_refspecs = vec![];
    if !proposal_refspecs.is_empty() {
        let all_proposals = get_all_proposals(git_repo, repo_ref).await?;
        let current_user = get_curent_user(git_repo)?;

        for refspec in &proposal_refspecs {
            let (from, to) = refspec_to_from_to(refspec).unwrap();
            let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;

            if let Some((_, (proposal, patches))) =
                find_proposal_and_patches_by_branch_name(to, &all_proposals, &current_user)
            {
                if [repo_ref.maintainers.clone(), vec![proposal.author()]]
                    .concat()
                    .contains(&user_ref.public_key)
                {
                    if refspec.starts_with('+') {
                        // force push
                        let (_, main_tip) = git_repo.get_main_or_master_branch()?;
                        let (mut ahead, _) =
                            git_repo.get_commits_ahead_behind(&main_tip, &tip_of_pushed_branch)?;
                        ahead.reverse();
                        for patch in generate_cover_letter_and_patch_events(
                            None,
                            git_repo,
                            &ahead,
                            &signer,
                            repo_ref,
                            &Some(proposal.id().to_string()),
                            &[],
                        )
                        .await?
                        {
                            events.push(patch);
                        }
                    } else {
                        // fast forward push
                        let tip_patch = patches.first().unwrap();
                        let tip_of_proposal = get_commit_id_from_patch(tip_patch)?;
                        let tip_of_proposal_commit =
                            git_repo.get_commit_or_tip_of_reference(&tip_of_proposal)?;

                        let (mut ahead, behind) = git_repo.get_commits_ahead_behind(
                            &tip_of_proposal_commit,
                            &tip_of_pushed_branch,
                        )?;
                        if behind.is_empty() {
                            let thread_id = if let Ok(root_event_id) = get_event_root(tip_patch) {
                                root_event_id
                            } else {
                                // tip patch is the root proposal
                                tip_patch.id()
                            };
                            let mut parent_patch = tip_patch.clone();
                            ahead.reverse();
                            for (i, commit) in ahead.iter().enumerate() {
                                let new_patch = generate_patch_event(
                                    git_repo,
                                    &git_repo.get_root_commit()?,
                                    commit,
                                    Some(thread_id),
                                    &signer,
                                    repo_ref,
                                    Some(parent_patch.id()),
                                    Some((
                                        (patches.len() + i + 1).try_into().unwrap(),
                                        (patches.len() + ahead.len()).try_into().unwrap(),
                                    )),
                                    None,
                                    &None,
                                    &[],
                                )
                                .await
                                .context("cannot make patch event from commit")?;
                                events.push(new_patch.clone());
                                parent_patch = new_patch;
                            }
                        } else {
                            // we shouldn't get here
                            term.write_line(
                                format!(
                                    "WARNING: failed to push {from} as nostr proposal. Try and force push ",
                                )
                                .as_str(),
                            )
                            .unwrap();
                            println!(
                                "error {to} cannot fastforward as newer patches found on proposal"
                            );
                            rejected_proposal_refspecs.push(refspec.to_string());
                        }
                    }
                } else {
                    println!(
                        "error {to} permission denied. you are not the proposal author or a repo maintainer"
                    );
                    rejected_proposal_refspecs.push(refspec.to_string());
                }
            } else {
                // TODO new proposal / couldn't find exisiting proposal
                let (_, main_tip) = git_repo.get_main_or_master_branch()?;
                let (mut ahead, _) =
                    git_repo.get_commits_ahead_behind(&main_tip, &tip_of_pushed_branch)?;
                ahead.reverse();
                for patch in generate_cover_letter_and_patch_events(
                    None,
                    git_repo,
                    &ahead,
                    &signer,
                    repo_ref,
                    &None,
                    &[],
                )
                .await?
                {
                    events.push(patch);
                }
            }
        }
    }

    // TODO check whether tip of each branch pushed is on at least one git server
    // before broadcasting the nostr state
    if !events.is_empty() {
        send_events(
            client,
            git_repo.get_path()?,
            events,
            user_ref.relays.write(),
            repo_ref.relays.clone(),
            false,
            true,
        )
        .await?;
    }

    for refspec in &[git_server_refspecs.clone(), proposal_refspecs.clone()].concat() {
        if rejected_proposal_refspecs.contains(refspec) {
            continue;
        }
        let (_, to) = refspec_to_from_to(refspec)?;
        println!("ok {to}");
        update_remote_refs_pushed(
            &git_repo.git_repo,
            refspec,
            &decoded_nostr_url.original_string,
        )
        .context("could not update remote_ref locally")?;
    }

    // TODO make async - check gitlib2 callbacks work async

    for (git_server_url, remote_refspecs) in remote_refspecs {
        let remote_refspecs = remote_refspecs
            .iter()
            .filter(|refspec| git_server_refspecs.contains(refspec))
            .cloned()
            .collect::<Vec<String>>();
        if !refspecs.is_empty() {
            let _ = push_to_remote(
                git_repo,
                &git_server_url,
                decoded_nostr_url,
                &remote_refspecs,
                &term,
            );
        }
    }
    println!();
    Ok(())
}

fn push_to_remote(
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    remote_refspecs: &[String],
    term: &Term,
) -> Result<()> {
    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt = get_write_protocols_to_try(git_repo, &server_url, decoded_nostr_url);

    let mut failed_protocols = vec![];
    let mut success = false;

    for protocol in &protocols_to_attempt {
        term.write_line(format!("push: {} over {protocol}...", server_url.short_name(),).as_str())?;

        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;

        if let Err(error) = push_to_remote_url(git_repo, &formatted_url, remote_refspecs, term) {
            term.write_line(
                format!("push: {formatted_url} failed over {protocol}: {error}").as_str(),
            )?;
            failed_protocols.push(protocol);
            if push_error_is_not_authentication_failure(&error) {
                break;
            }
        } else {
            success = true;
            if !failed_protocols.is_empty() {
                term.write_line(format!("push: succeeded over {protocol}").as_str())?;
                let _ = set_protocol_preference(git_repo, protocol, &server_url, &Direction::Push);
            }
            break;
        }
    }
    if success {
        Ok(())
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

fn push_to_remote_url(
    git_repo: &Repo,
    git_server_url: &str,
    remote_refspecs: &[String],
    term: &Term,
) -> Result<()> {
    let git_config = git_repo.git_repo.config()?;
    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_url)?;
    let auth = GitAuthenticator::default();
    let mut push_options = git2::PushOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    let push_reporter = Arc::new(Mutex::new(PushReporter::new(term)));

    remote_callbacks.credentials(auth.credentials(&git_config));

    remote_callbacks.push_update_reference({
        let push_reporter = Arc::clone(&push_reporter);
        move |name, error| {
            let mut reporter = push_reporter.lock().unwrap();
            if let Some(error) = error {
                let existing_lines = reporter.count_all_existing_lines();
                reporter.update_reference_errors.push(format!(
                    "WARNING: {} failed to push {name} error: {error}",
                    get_short_git_server_name(git_repo, git_server_url),
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
                let msg = if update.dst().is_zero() {
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
    git_server_remote.push(remote_refspecs, Some(&mut push_options))?;
    let _ = git_server_remote.disconnect();
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
#[allow(clippy::float_cmp)]
#[allow(clippy::needless_pass_by_value)]
fn report_on_transfer_progress(
    current: usize,
    total: usize,
    bytes: usize,
    start_time: &Instant,
    end_time: &Option<Instant>,
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

struct PushReporter<'a> {
    remote_msgs: Vec<String>,
    negotiation: Vec<String>,
    transfer_progress_msgs: Vec<String>,
    update_reference_errors: Vec<String>,
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
            &self.end_time,
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

type HashMapUrlRefspecs = HashMap<String, Vec<String>>;

#[allow(clippy::too_many_lines)]
fn create_rejected_refspecs_and_remotes_refspecs(
    term: &console::Term,
    git_repo: &Repo,
    refspecs: &Vec<String>,
    nostr_state: &HashMap<String, String>,
    list_outputs: &HashMap<String, HashMap<String, String>>,
) -> Result<(HashMapUrlRefspecs, HashMapUrlRefspecs)> {
    let mut refspecs_for_remotes = HashMap::new();

    let mut rejected_refspecs: HashMapUrlRefspecs = HashMap::new();

    for (url, remote_state) in list_outputs {
        let short_name = get_short_git_server_name(git_repo, url);
        let mut refspecs_for_remote = vec![];
        for refspec in refspecs {
            let (from, to) = refspec_to_from_to(refspec)?;
            let nostr_value = nostr_state.get(to);
            let remote_value = remote_state.get(to);
            if from.is_empty() {
                if remote_value.is_some() {
                    // delete remote branch
                    refspecs_for_remote.push(refspec.clone());
                }
                continue;
            }
            let from_tip = git_repo.get_commit_or_tip_of_reference(from)?;
            if let Some(nostr_value) = nostr_value {
                if let Some(remote_value) = remote_value {
                    if nostr_value.eq(remote_value) {
                        // in sync - existing branch at same state
                        let is_remote_tip_ancestor_of_commit = if let Ok(remote_value_tip) =
                            git_repo.get_commit_or_tip_of_reference(remote_value)
                        {
                            if let Ok((_, behind)) =
                                git_repo.get_commits_ahead_behind(&remote_value_tip, &from_tip)
                            {
                                behind.is_empty()
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if is_remote_tip_ancestor_of_commit {
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            // this is a force push so we need to force push to git server too
                            if refspec.starts_with('+') {
                                refspecs_for_remote.push(refspec.clone());
                            } else {
                                refspecs_for_remote.push(format!("+{refspec}"));
                            }
                        }
                    } else if let Ok(remote_value_tip) =
                        git_repo.get_commit_or_tip_of_reference(remote_value)
                    {
                        if from_tip.eq(&remote_value_tip) {
                            // remote already at correct state
                            term.write_line(
                                format!("{short_name} {to} already up-to-date").as_str(),
                            )?;
                        }
                        let (ahead_of_local, behind_local) =
                            git_repo.get_commits_ahead_behind(&from_tip, &remote_value_tip)?;
                        if ahead_of_local.is_empty() {
                            // can soft push
                            refspecs_for_remote.push(refspec.clone());
                        } else {
                            // cant soft push
                            let (ahead_of_nostr, behind_nostr) = git_repo
                                .get_commits_ahead_behind(
                                    &git_repo.get_commit_or_tip_of_reference(nostr_value)?,
                                    &remote_value_tip,
                                )?;
                            if ahead_of_nostr.is_empty() {
                                // ancestor of nostr and we are force pushing anyway...
                                refspecs_for_remote.push(refspec.clone());
                            } else {
                                rejected_refspecs
                                    .entry(refspec.to_string())
                                    .and_modify(|a| a.push(url.to_string()))
                                    .or_insert(vec![url.to_string()]);
                                term.write_line(
                                    format!(
                                        "ERROR: {short_name} {to} conflicts with nostr ({} ahead {} behind) and local ({} ahead {} behind). either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote",
                                        ahead_of_nostr.len(),
                                        behind_nostr.len(),
                                        ahead_of_local.len(),
                                        behind_local.len(),
                                    ).as_str(),
                                )?;
                            }
                        };
                    } else {
                        // remote_value oid is not present locally
                        // TODO can we download the remote reference?

                        // cant soft push
                        rejected_refspecs
                            .entry(refspec.to_string())
                            .and_modify(|a| a.push(url.to_string()))
                            .or_insert(vec![url.to_string()]);
                        term.write_line(
                            format!("ERROR: {short_name} {to} conflicts with nostr and is not an ancestor of local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote").as_str(),
                        )?;
                    }
                } else {
                    // existing nostr branch not on remote
                    // report - creating new branch
                    term.write_line(
                        format!(
                            "{short_name} {to} doesn't exist and will be added as a new branch"
                        )
                        .as_str(),
                    )?;
                    refspecs_for_remote.push(refspec.clone());
                }
            } else if let Some(remote_value) = remote_value {
                // new to nostr but on remote
                if let Ok(remote_value_tip) = git_repo.get_commit_or_tip_of_reference(remote_value)
                {
                    let (ahead, behind) =
                        git_repo.get_commits_ahead_behind(&from_tip, &remote_value_tip)?;
                    if behind.is_empty() {
                        // can soft push
                        refspecs_for_remote.push(refspec.clone());
                    } else {
                        // cant soft push
                        rejected_refspecs
                            .entry(refspec.to_string())
                            .and_modify(|a| a.push(url.to_string()))
                            .or_insert(vec![url.to_string()]);
                        term.write_line(
                                    format!(
                                        "ERROR: {short_name} already contains {to} {} ahead and {} behind local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote",
                                        ahead.len(),
                                        behind.len(),
                                    ).as_str(),
                                )?;
                    }
                } else {
                    // havn't fetched oid from remote
                    // TODO fetch oid from remote
                    // cant soft push
                    rejected_refspecs
                        .entry(refspec.to_string())
                        .and_modify(|a| a.push(url.to_string()))
                        .or_insert(vec![url.to_string()]);
                    term.write_line(
                        format!("ERROR: {short_name} already contains {to} at {remote_value} which is not an ancestor of local branch. either:\r\n  1. pull from that git server and resolve\r\n  2. force push your branch to the git server before pushing to nostr remote").as_str(),
                    )?;
                }
            } else {
                // in sync - new branch
                refspecs_for_remote.push(refspec.clone());
            }
        }
        if !refspecs_for_remote.is_empty() {
            refspecs_for_remotes.insert(url.to_string(), refspecs_for_remote);
        }
    }

    // remove rejected refspecs so they dont get pushed to some remotes
    let mut remotes_refspecs_without_rejected = HashMap::new();
    for (url, value) in &refspecs_for_remotes {
        remotes_refspecs_without_rejected.insert(
            url.to_string(),
            value
                .iter()
                .filter(|refspec| !rejected_refspecs.contains_key(*refspec))
                .cloned()
                .collect(),
        );
    }
    Ok((rejected_refspecs, remotes_refspecs_without_rejected))
}

fn generate_updated_state(
    git_repo: &Repo,
    existing_state: &HashMap<String, String>,
    refspecs: &Vec<String>,
) -> Result<HashMap<String, String>> {
    let mut new_state = existing_state.clone();

    for refspec in refspecs {
        let (from, to) = refspec_to_from_to(refspec)?;
        if from.is_empty() {
            // delete
            new_state.remove(to);
            if to.contains("refs/tags") {
                new_state.remove(&format!("{to}{}", "^{}"));
            }
        } else if to.contains("refs/tags") {
            new_state.insert(
                format!("{to}{}", "^{}"),
                git_repo
                    .get_commit_or_tip_of_reference(from)
                    .unwrap()
                    .to_string(),
            );
            new_state.insert(
                to.to_string(),
                git_repo
                    .git_repo
                    .find_reference(to)
                    .unwrap()
                    .peel(git2::ObjectType::Tag)
                    .unwrap()
                    .id()
                    .to_string(),
            );
        } else {
            // add or update
            new_state.insert(
                to.to_string(),
                git_repo
                    .get_commit_or_tip_of_reference(from)
                    .unwrap()
                    .to_string(),
            );
        }
    }
    Ok(new_state)
}

async fn get_merged_status_events(
    term: &console::Term,
    repo_ref: &RepoRef,
    git_repo: &Repo,
    remote_nostr_url: &str,
    signer: &NostrSigner,
    refspecs_to_git_server: &Vec<String>,
) -> Result<Vec<Event>> {
    let mut events = vec![];
    for refspec in refspecs_to_git_server {
        let (from, to) = refspec_to_from_to(refspec)?;
        if to.eq("refs/heads/main") || to.eq("refs/heads/master") {
            let tip_of_pushed_branch = git_repo.get_commit_or_tip_of_reference(from)?;
            let Ok(tip_of_remote_branch) = git_repo.get_commit_or_tip_of_reference(
                &refspec_remote_ref_name(&git_repo.git_repo, refspec, remote_nostr_url)?,
            ) else {
                // branch not on remote
                continue;
            };
            let (ahead, _) =
                git_repo.get_commits_ahead_behind(&tip_of_remote_branch, &tip_of_pushed_branch)?;
            for commit_hash in ahead {
                let commit = git_repo.git_repo.find_commit(sha1_to_oid(&commit_hash)?)?;
                if commit.parent_count() > 1 {
                    // merge commit
                    for parent in commit.parents() {
                        // lookup parent id
                        let commit_events = get_events_from_cache(
                            git_repo.get_path()?,
                            vec![
                                nostr::Filter::default()
                                    .kind(nostr::Kind::GitPatch)
                                    .reference(parent.id().to_string()),
                            ],
                        )
                        .await?;
                        if let Some(commit_event) = commit_events.iter().find(|e| {
                            e.tags.iter().any(|t| {
                                t.as_vec()[0].eq("commit")
                                    && t.as_vec()[1].eq(&parent.id().to_string())
                            })
                        }) {
                            let (proposal_id, revision_id) =
                                get_proposal_and_revision_root_from_patch(git_repo, commit_event)
                                    .await?;
                            term.write_line(
                                format!(
                                    "merge commit {}: create nostr proposal status event",
                                    &commit.id().to_string()[..7],
                                )
                                .as_str(),
                            )?;

                            events.push(
                                create_merge_status(
                                    signer,
                                    repo_ref,
                                    &get_event_from_cache_by_id(git_repo, &proposal_id).await?,
                                    &if let Some(revision_id) = revision_id {
                                        Some(
                                            get_event_from_cache_by_id(git_repo, &revision_id)
                                                .await?,
                                        )
                                    } else {
                                        None
                                    },
                                    &commit_hash,
                                    commit_event.id(),
                                )
                                .await?,
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(events)
}

async fn create_merge_status(
    signer: &NostrSigner,
    repo_ref: &RepoRef,
    proposal: &Event,
    revision: &Option<Event>,
    merge_commit: &Sha1Hash,
    merged_patch: EventId,
) -> Result<Event> {
    let mut public_keys = repo_ref
        .maintainers
        .iter()
        .copied()
        .collect::<HashSet<PublicKey>>();
    public_keys.insert(proposal.author());
    if let Some(revision) = revision {
        public_keys.insert(revision.author());
    }
    sign_event(
        EventBuilder::new(
            nostr::event::Kind::GitStatusApplied,
            String::new(),
            [
                vec![
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                        vec!["git proposal merged / applied".to_string()],
                    ),
                    Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: proposal.id(),
                        relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::new),
                        marker: Some(Marker::Root),
                        public_key: None,
                    }),
                    Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: merged_patch,
                        relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::new),
                        marker: Some(Marker::Mention),
                        public_key: None,
                    }),
                ],
                if let Some(revision) = revision {
                    vec![Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: revision.id(),
                        relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::new),
                        marker: Some(Marker::Root),
                        public_key: None,
                    })]
                } else {
                    vec![]
                },
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| Tag::coordinate(c.clone()))
                    .collect::<Vec<Tag>>(),
                vec![
                    Tag::from_standardized(nostr::TagStandard::Reference(
                        repo_ref.root_commit.to_string(),
                    )),
                    Tag::from_standardized(nostr::TagStandard::Reference(format!(
                        "{merge_commit}"
                    ))),
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("merge-commit-id")),
                        vec![format!("{merge_commit}")],
                    ),
                ],
            ]
            .concat(),
        ),
        signer,
    )
    .await
}

async fn get_proposal_and_revision_root_from_patch(
    git_repo: &Repo,
    patch: &Event,
) -> Result<(EventId, Option<EventId>)> {
    let proposal_or_revision = if patch.tags.iter().any(|t| t.as_vec()[1].eq("root")) {
        patch.clone()
    } else {
        let proposal_or_revision_id = EventId::parse(
            if let Some(t) = patch.tags.iter().find(|t| t.is_root()) {
                t.clone()
            } else if let Some(t) = patch.tags.iter().find(|t| t.is_reply()) {
                t.clone()
            } else {
                Tag::event(patch.id())
            }
            .as_vec()[1]
                .clone(),
        )?;

        get_events_from_cache(
            git_repo.get_path()?,
            vec![nostr::Filter::default().id(proposal_or_revision_id)],
        )
        .await?
        .first()
        .unwrap()
        .clone()
    };

    if !proposal_or_revision.kind().eq(&Kind::GitPatch) {
        bail!("thread root is not a git patch");
    }

    if proposal_or_revision
        .tags
        .iter()
        .any(|t| t.as_vec()[1].eq("revision-root"))
    {
        Ok((
            EventId::parse(
                proposal_or_revision
                    .tags
                    .iter()
                    .find(|t| t.is_reply())
                    .unwrap()
                    .as_vec()[1]
                    .clone(),
            )?,
            Some(proposal_or_revision.id()),
        ))
    } else {
        Ok((proposal_or_revision.id(), None))
    }
}

fn update_remote_refs_pushed(
    git_repo: &Repository,
    refspec: &str,
    nostr_remote_url: &str,
) -> Result<()> {
    let (from, _) = refspec_to_from_to(refspec)?;

    let target_ref_name = refspec_remote_ref_name(git_repo, refspec, nostr_remote_url)?;

    if from.is_empty() {
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.delete()?;
        }
    } else {
        let commit = reference_to_commit(git_repo, from)
            .context(format!("cannot get commit of reference {from}"))?;
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.set_target(commit, "updated by nostr remote helper")?;
        } else {
            git_repo.reference(
                &target_ref_name,
                commit,
                false,
                "created by nostr remote helper",
            )?;
        }
    }
    Ok(())
}

fn refspec_to_from_to(refspec: &str) -> Result<(&str, &str)> {
    if !refspec.contains(':') {
        bail!(
            "refspec should contain a colon (:) but consists of: {}",
            refspec
        );
    }
    let parts = refspec.split(':').collect::<Vec<&str>>();
    Ok((
        if parts.first().unwrap().starts_with('+') {
            &parts.first().unwrap()[1..]
        } else {
            parts.first().unwrap()
        },
        parts.get(1).unwrap(),
    ))
}

fn refspec_remote_ref_name(
    git_repo: &Repository,
    refspec: &str,
    nostr_remote_url: &str,
) -> Result<String> {
    let (_, to) = refspec_to_from_to(refspec)?;
    let nostr_remote = git_repo
        .find_remote(&get_remote_name_by_url(git_repo, nostr_remote_url)?)
        .context("we should have just located this remote")?;
    Ok(format!(
        "refs/remotes/{}/{}",
        nostr_remote.name().context("remote should have a name")?,
        to.replace("refs/heads/", ""), /* TODO only replace if it begins with this
                                        * TODO what about tags? */
    ))
}

fn reference_to_commit(git_repo: &Repository, reference: &str) -> Result<Oid> {
    Ok(git_repo
        .find_reference(reference)
        .context(format!("cannot find reference: {reference}"))?
        .peel_to_commit()
        .context(format!("cannot get commit from reference: {reference}"))?
        .id())
}

// this maybe a commit id or a ref: pointer
fn reference_to_ref_value(git_repo: &Repository, reference: &str) -> Result<String> {
    let reference_obj = git_repo
        .find_reference(reference)
        .context(format!("cannot find reference: {reference}"))?;
    if let Some(symref) = reference_obj.symbolic_target() {
        Ok(symref.to_string())
    } else {
        Ok(reference_obj
            .peel_to_commit()
            .context(format!("cannot get commit from reference: {reference}"))?
            .id()
            .to_string())
    }
}

fn get_refspecs_from_push_batch(stdin: &Stdin, initial_refspec: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut refspecs = vec![initial_refspec.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["push", spec] => {
                refspecs.push((*spec).to_string());
            }
            [] => break,
            _ => {
                bail!("after a `push` command we are only expecting another push or an empty line")
            }
        }
    }
    Ok(refspecs)
}

trait BuildRepoState {
    async fn build(
        identifier: String,
        state: HashMap<String, String>,
        signer: &NostrSigner,
    ) -> Result<RepoState>;
}
impl BuildRepoState for RepoState {
    async fn build(
        identifier: String,
        state: HashMap<String, String>,
        signer: &NostrSigner,
    ) -> Result<RepoState> {
        let mut tags = vec![Tag::identifier(identifier.clone())];
        for (name, value) in &state {
            tags.push(Tag::custom(
                nostr_sdk::TagKind::Custom(name.into()),
                vec![value.clone()],
            ));
        }
        let event = sign_event(EventBuilder::new(STATE_KIND, "", tags), signer).await?;
        Ok(RepoState {
            identifier,
            state,
            event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod refspec_to_from_to {
        use super::*;

        #[test]
        fn trailing_plus_stripped() {
            let (from, _) = refspec_to_from_to("+testing:testingb").unwrap();
            assert_eq!(from, "testing");
        }
    }
}
