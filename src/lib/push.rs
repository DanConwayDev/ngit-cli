use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use auth_git2::GitAuthenticator;
use console::Term;
use nostr::{
    event::{Event, EventBuilder, Kind, Tag, TagStandard, UnsignedEvent},
    hashes::sha1::Hash as Sha1Hash,
    key::PublicKey,
    nips::{nip01::Coordinate, nip10::Marker, nip19::Nip19Coordinate},
    signer::NostrSigner,
};

use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms, PromptInputParms,
        count_lines_per_msg_vec, multi_select_with_custom_value, show_multi_input_prompt_success,
    },
    client::{Connect, get_repo_ref_from_cache, send_events, sign_draft_event, sign_event},
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded},
        oid_to_shorthand_string,
    },
    git_events::{KIND_PULL_REQUEST_UPDATE, generate_unsigned_pr_or_update_event},
    login::user::UserRef,
    repo_ref::{
        RepoRef, format_grasp_server_url_as_clone_url, format_grasp_server_url_as_relay_url,
        is_grasp_server_clone_url, is_grasp_server_in_list, normalize_grasp_server_url,
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
) -> Result<HashMap<String, Option<String>>> {
    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_write_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

    let mut failed_protocols = vec![];
    let mut success = false;
    let mut ref_updates = HashMap::new();

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
    remote_refspecs: &[String],
    term: &Term,
) -> Result<HashMap<String, Option<String>>> {
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
            reporter
                .ref_updates
                .insert(name.to_string(), error.map(|s| s.to_string()));
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
    user_ref: &mut UserRef,
    root_proposal: Option<&Event>,
    title_description_overide: &Option<(String, String)>,
    signer: &Arc<dyn NostrSigner>,
    interactive: bool,
    term: &Term,
) -> Result<Vec<Event>> {
    let git_repo_path = git_repo.get_path()?;
    let mut to_try = vec![];
    let mut tried = vec![];
    let repo_grasps = repo_ref.grasp_servers();
    // if the user already has a fork, or is a maintainer, use those git servers
    let mut user_repo_ref = get_repo_ref_from_cache(
        Some(git_repo_path),
        &Nip19Coordinate {
            coordinate: Coordinate {
                kind: nostr::event::Kind::GitRepoAnnouncement,
                public_key: user_ref.public_key,
                identifier: repo_ref.identifier.clone(),
            },
            relays: vec![],
        },
    )
    .await
    .ok();
    if let Some(user_repo_ref) = &user_repo_ref {
        for url in &user_repo_ref.git_server {
            if CloneUrl::from_str(url).is_ok() {
                to_try.push(url.clone());
            }
        }
    }
    if !to_try.is_empty() || !repo_grasps.is_empty() {
        println!(
            "pushing proposal refs to {}",
            if repo_ref.maintainers.contains(&user_ref.public_key) {
                "repository git servers"
            } else if to_try.is_empty() {
                "repository grasp servers"
            } else if repo_grasps.is_empty() {
                "the git servers listed in your fork"
            } else {
                "the git servers listed in your fork and repository grasp servers"
            }
        );
    } else {
        println!(
            "The repository doesn't list a grasp server which would otherwise be used to submit your proposal as nostr Pull Request."
        );
    }
    // also use repo grasp servers
    for url in &repo_ref.git_server {
        if is_grasp_server_in_list(url, &repo_grasps) && !to_try.contains(url) {
            to_try.push(url.clone());
        }
    }

    let mut git_ref = None;
    let events = loop {
        let (events, _server_responses) = push_refs_and_generate_pr_or_pr_update_event(
            git_repo,
            repo_ref,
            tip,
            user_ref,
            root_proposal,
            title_description_overide,
            &to_try,
            git_ref.clone(),
            signer,
            term,
        )
        .await?;
        for url in to_try {
            tried.push(url);
        }
        to_try = vec![];
        if let Some(events) = events {
            break events;
        }
        // fallback to creating user personal-fork on their grasp servers
        let untried_user_grasp_servers: Vec<String> = user_ref
            .grasp_list
            .urls
            .iter()
            .map(std::string::ToString::to_string)
            .filter(|g| {
                // is a grasp server not in list of tried
                !is_grasp_server_in_list(g, &tried)
            })
            .collect();

        if untried_user_grasp_servers.is_empty() {
            if !interactive {
                if repo_grasps.is_empty() {
                    bail!(
                        "failed to write PR data. nostr repo doesnt lists any grasp servers which allow you to write PR branches. run `ngit send` to select an alternative git server to host your PR diff."
                    )
                }
                bail!(
                    "failed to write PR data to git servers associated with this nostr repo. run `ngit send` to select an alternative git server to host your PR diff."
                )
            }
            if Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_prompt("choose alternative git server")
                    .dont_report()
                    .with_choices(vec![
                        "choose grasp server(s)".to_string(),
                        "enter a git repo url with write permission".to_string(),
                    ])
                    .with_default(0),
            )? == 1
            {
                loop {
                    let clone_url = Interactor::default()
                        .input(
                            PromptInputParms::default()
                                .with_prompt("git repo url with write permission"),
                        )?
                        .clone();
                    if CloneUrl::from_str(&clone_url).is_ok() {
                        to_try.push(clone_url);
                        let mut git_ref_or_branch_name = Interactor::default()
                            .input(
                                PromptInputParms::default()
                                    .with_prompt("ref / branch name")
                                    .with_default(
                                        git_ref.unwrap_or("refs/nostr/<event-id>".to_string()),
                                    ),
                            )?
                            .clone();
                        if !git_ref_or_branch_name.starts_with("refs/") {
                            git_ref_or_branch_name = format!("refs/heads/{git_ref_or_branch_name}");
                        }
                        git_ref = Some(git_ref_or_branch_name);
                        break;
                    }
                    println!("invalid clone url");
                }
                continue;
            }
        }

        let mut new_grasp_server_events: Vec<Event> = vec![];

        let grasp_servers = if untried_user_grasp_servers.is_empty() {
            let default_choices: Vec<String> = client
                .get_grasp_default_set()
                .iter()
                .filter(|g| !is_grasp_server_in_list(g, &tried))
                .cloned()
                .collect();
            let selections = vec![true; default_choices.len()]; // all selected by default
            let grasp_servers = multi_select_with_custom_value(
                "alternative grasp server(s)",
                "grasp server",
                default_choices,
                selections,
                normalize_grasp_server_url,
            )?;
            show_multi_input_prompt_success("alternative grasp server(s)", &grasp_servers);
            if grasp_servers.is_empty() {
                // ask again
                continue;
            }
            let normalised_grasp_servers: Vec<String> = grasp_servers
                .iter()
                .filter_map(|g| normalize_grasp_server_url(g).ok())
                .collect();
            // if any grasp servers not listed in user grasp list prompt to update
            let grasp_servers_not_in_user_prefs: Vec<String> = normalised_grasp_servers
                .iter()
                .filter(|g| {
                    !user_ref.grasp_list.urls.contains(
                        // unwrap is safe as we constructed g
                        &nostr::Url::parse(&format_grasp_server_url_as_relay_url(g).unwrap())
                            .unwrap(),
                    )
                })
                .cloned()
                .collect();
            if !grasp_servers_not_in_user_prefs.is_empty()
                && Interactor::default().confirm(
                    PromptConfirmParms::default()
                        .with_prompt(
                            "add these to your list of prefered grasp servers?".to_string(),
                        )
                        .with_default(true),
                )?
            {
                for g in &normalised_grasp_servers {
                    let as_url = nostr::Url::parse(&format_grasp_server_url_as_relay_url(g)?)?;
                    if !user_ref.grasp_list.urls.contains(&as_url) {
                        user_ref.grasp_list.urls.push(as_url);
                    }
                }
                new_grasp_server_events.push(user_ref.grasp_list.to_event(signer).await?);
            }
            normalised_grasp_servers
        } else {
            untried_user_grasp_servers
        };
        println!(
            "{} personal-fork so we can push commits to your prefered grasp servers",
            if user_repo_ref.is_some() {
                "Updating"
            } else {
                "Creating a"
            },
        );

        let grasp_servers_as_personal_clone_url: Vec<String> = grasp_servers
            .iter()
            .filter_map(|g| {
                format_grasp_server_url_as_clone_url(g, &user_ref.public_key, &repo_ref.identifier)
                    .ok()
            })
            .collect();

        // create personal-fork / update existing user repo and add these grasp servers
        let updated_user_repo_ref = {
            if let Some(mut user_repo_ref) = user_repo_ref {
                for g in &grasp_servers_as_personal_clone_url {
                    user_repo_ref.add_grasp_server(g)?;
                }
                user_repo_ref
            } else {
                // clone repo_ref and reset as personal-fork
                let mut user_repo_ref = repo_ref.clone();
                user_repo_ref.trusted_maintainer = user_ref.public_key;
                user_repo_ref.maintainers = vec![user_ref.public_key];
                user_repo_ref.git_server = vec![];
                user_repo_ref.relays = vec![];
                if !user_repo_ref
                    .hashtags
                    .contains(&"personal-fork".to_string())
                {
                    user_repo_ref.hashtags.push("personal-fork".to_string());
                }
                user_repo_ref
            }
        };
        // pubish event to my-relays and my-fork-relays
        new_grasp_server_events.push(updated_user_repo_ref.to_event(signer).await?);
        send_events(
            client,
            Some(git_repo_path),
            new_grasp_server_events,
            user_ref.relays.write(),
            updated_user_repo_ref.relays.clone(),
            #[cfg(test)]
            true,
            #[cfg(not(test))]
            false,
            false,
        )
        .await?;
        user_repo_ref = Some(updated_user_repo_ref);
        // wait a few seconds
        let countdown_start = 5;
        let term = console::Term::stdout();
        for i in (1..=countdown_start).rev() {
            term.write_line(
                format!("waiting {i}s grasp servers to create your repo before we push your data")
                    .as_str(),
            )?;
            thread::sleep(Duration::new(1, 0)); // Sleep for 1 second
            term.clear_last_lines(1)?;
        }
        term.flush().unwrap(); // Ensure the output is flushed to the terminal

        // add grasp servers to to_try
        for url in grasp_servers_as_personal_clone_url {
            to_try.push(url);
        }
        // the loop with continue with the grasp servers
    };
    println!(
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

        let res = if is_grasp_server_clone_url(clone_url) {
            push_to_remote_url(git_repo, clone_url, &[refspec], term)
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
            )
        };

        match res {
            Err(error) => {
                let normalized_url = normalize_grasp_server_url(clone_url)?;
                term.write_line(&format!(
                    "push: error sending commit data to {normalized_url}: {error}"
                ))?;
                responses.push((clone_url.clone(), Err(anyhow!(error))));
            }
            Ok(ref_updates) => {
                let normalized_url = normalize_grasp_server_url(clone_url)?;
                if let Some((_, Some(error))) = ref_updates.iter().next() {
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
