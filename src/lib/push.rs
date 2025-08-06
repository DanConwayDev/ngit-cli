use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Result, anyhow};
use auth_git2::GitAuthenticator;
use console::Term;
use nostr::{
    event::{Event, EventBuilder, Kind, Tag, TagStandard, UnsignedEvent},
    hashes::sha1::Hash as Sha1Hash,
    key::PublicKey,
    nips::nip10::Marker,
    signer::NostrSigner,
};

use crate::{
    cli_interactor::count_lines_per_msg_vec,
    client::{sign_draft_event, sign_event},
    git::{
        Repo,
        nostr_url::{CloneUrl, NostrUrlDecoded},
        oid_to_shorthand_string,
    },
    git_events::generate_unsigned_pr_or_update_event,
    login::user::UserRef,
    repo_ref::{RepoRef, normalize_grasp_server_url},
    utils::{
        Direction, get_short_git_server_name, get_write_protocols_to_try, join_with_and,
        set_protocol_preference,
    },
};

pub fn push_to_remote(
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    remote_refspecs: &[String],
    term: &Term,
    is_grasp_server: bool,
) -> Result<()> {
    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_write_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

    let mut failed_protocols = vec![];
    let mut success = false;

    for protocol in &protocols_to_attempt {
        term.write_line(format!("push: {} over {protocol}...", server_url.short_name(),).as_str())?;

        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;

        match push_to_remote_url(git_repo, &formatted_url, remote_refspecs, term) {
            Err(error) => {
                term.write_line(
                    format!("push: {formatted_url} failed over {protocol}: {error}").as_str(),
                )?;
                failed_protocols.push(protocol);
            }
            Ok(failed_refs) => {
                if let Some((_, error)) = failed_refs.iter().next() {
                    term.write_line(
                        format!("push: {formatted_url} failed over {protocol}: {error}").as_str(),
                    )?;
                    failed_protocols.push(protocol);
                } else {
                    success = true;
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
                }
            }
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

// returns failed refs as a HashMaps of failed refspec and their error
pub fn push_to_remote_url(
    git_repo: &Repo,
    git_server_url: &str,
    remote_refspecs: &[String],
    term: &Term,
) -> Result<HashMap<String, String>> {
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
                reporter
                    .failed_refs
                    .insert(name.to_string(), error.to_string());
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
    let reporter = push_reporter.lock().unwrap();
    Ok(reporter.failed_refs.clone())
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
    failed_refs: HashMap<String, String>,
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
            failed_refs: HashMap::new(),
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
pub async fn push_refs_and_generate_pr_or_pr_update_event(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    tip: &Sha1Hash,
    user_ref: &UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    servers: &[String],
    git_ref: Option<String>,
    signer: &Arc<dyn NostrSigner>,
    term: &Term,
) -> Result<(Option<Vec<Event>>, Vec<(String, Result<()>)>)> {
    let mut responses: Vec<(String, Result<()>)> = vec![];

    let mut unsigned_pr_event: Option<UnsignedEvent> = None;
    for clone_url in servers {
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
                &[clone_url],
                &[],
            )?
        };

        let git_ref_used = git_ref
            .clone()
            .unwrap_or("refs/nostr/<event-id>".to_string())
            .replace("<event-id>", &draft_pr_event.id().to_string());

        let refspec = format!("{tip}:{git_ref_used}");

        match push_to_remote_url(git_repo, clone_url, &[refspec], term) {
            Err(error) => {
                let normalized_url = normalize_grasp_server_url(clone_url)?;
                term.write_line(&format!(
                    "push: error sending commit data to {normalized_url}: {error}"
                ))?;
                responses.push((clone_url.clone(), Err(anyhow!(error))));
            }
            Ok(failed_refs) => {
                let normalized_url = normalize_grasp_server_url(clone_url)?;
                if let Some((_, error)) = failed_refs.iter().next() {
                    term.write_line(&format!(
                        "push: error sending commit data to {normalized_url}: {error}"
                    ))?;
                    responses.push((clone_url.clone(), Err(anyhow!(error.clone()))));
                } else {
                    responses.push((clone_url.clone(), Ok(())));
                    term.write_line(&format!("push: commit data sent to {normalized_url}"))?;
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

async fn create_close_status_for_original_patch(
    signer: &Arc<dyn NostrSigner>,
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
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                        vec![
                            "Git patch closed as forthcoming update is too large. Replacing with Pull Request"
                                .to_string(),
                        ],
                    ),
                    Tag::from_standardized(nostr::TagStandard::Event {
                        event_id: proposal.id,
                        relay_url: repo_ref.relays.first().cloned(),
                        marker: Some(Marker::Root),
                        public_key: None,
                        uppercase: false,
                    }),
                ],
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        Tag::from_standardized(TagStandard::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_url: c.relays.first().cloned(),
                            uppercase: false,
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![
                    Tag::from_standardized(nostr::TagStandard::Reference(
                        repo_ref.root_commit.to_string(),
                    )),
                ],
            ]
            .concat(),
        ),
        signer,
        "close status for original patch".to_string(),
    )
    .await
}
