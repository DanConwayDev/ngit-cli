use core::str;
use std::{
    io::Stdin,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{anyhow, bail, Result};
use auth_git2::GitAuthenticator;
use git2::{Progress, Repository};
use ngit::{
    git::{
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        utils::check_ssh_keys,
        Repo, RepoActions,
    },
    git_events::tag_value,
    login::get_curent_user,
    repo_ref::RepoRef,
};

use crate::utils::{
    fetch_or_list_error_is_not_authentication_failure, find_proposal_and_patches_by_branch_name,
    get_oids_from_fetch_batch, get_open_proposals, get_read_protocols_to_try, join_with_and,
};

pub async fn run_fetch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    decoded_nostr_url: &NostrUrlDecoded,
    stdin: &Stdin,
    oid: &str,
    refstr: &str,
) -> Result<()> {
    let mut fetch_batch = get_oids_from_fetch_batch(stdin, oid, refstr)?;

    let oids_from_git_servers = fetch_batch
        .iter()
        .filter(|(refstr, _)| !refstr.contains("refs/heads/pr/"))
        .map(|(_, oid)| oid.clone())
        .collect::<Vec<String>>();

    let mut errors = vec![];
    let term = console::Term::stderr();

    for git_server_url in &repo_ref.git_server {
        let term = console::Term::stderr();
        if let Err(error) = fetch_from_git_server(
            &git_repo.git_repo,
            &oids_from_git_servers,
            git_server_url,
            decoded_nostr_url,
            &term,
        ) {
            errors.push(error);
        } else {
            break;
        }
    }

    if oids_from_git_servers
        .iter()
        .any(|oid| !git_repo.does_commit_exist(oid).unwrap())
        && !errors.is_empty()
    {
        bail!(
            "fetch: failed to fetch objects in nostr state event from:\r\n{}",
            errors
                .iter()
                .map(|e| format!(" - {e}"))
                .collect::<Vec<String>>()
                .join("\r\n")
        );
    }

    fetch_batch.retain(|refstr, _| refstr.contains("refs/heads/pr/"));

    if !fetch_batch.is_empty() {
        let open_proposals = get_open_proposals(git_repo, repo_ref).await?;

        let current_user = get_curent_user(git_repo)?;

        for (refstr, oid) in fetch_batch {
            if let Some((_, (_, patches))) =
                find_proposal_and_patches_by_branch_name(&refstr, &open_proposals, &current_user)
            {
                if !git_repo.does_commit_exist(&oid)? {
                    let mut patches_ancestor_first = patches.clone();
                    patches_ancestor_first.reverse();
                    if git_repo.does_commit_exist(&tag_value(
                        patches_ancestor_first.first().unwrap(),
                        "parent-commit",
                    )?)? {
                        for patch in &patches_ancestor_first {
                            git_repo.create_commit_from_patch(patch)?;
                        }
                    } else {
                        term.write_line(
                            format!("WARNING: cannot find parent commit for {refstr}").as_str(),
                        )?;
                    }
                }
            } else {
                term.write_line(format!("WARNING: cannot find proposal for {refstr}").as_str())?;
            }
        }
    }

    term.flush()?;
    println!();
    Ok(())
}

fn fetch_from_git_server(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    term: &console::Term,
) -> Result<()> {
    let server_url = git_server_url.parse::<CloneUrl>()?;

    let protocols_to_attempt = get_read_protocols_to_try(&server_url, decoded_nostr_url);

    let mut failed_protocols = vec![];
    let mut success = false;
    for protocol in &protocols_to_attempt {
        term.write_line(
            format!("fetching {} over {protocol}...", server_url.short_name(),).as_str(),
        )?;

        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;
        let res = fetch_from_git_server_url(
            git_repo,
            oids,
            &formatted_url,
            [ServerProtocol::UnauthHttps, ServerProtocol::UnauthHttp].contains(protocol),
            term,
        );
        if let Err(error) = res {
            term.write_line(
                format!("fetch: {formatted_url} failed over {protocol}: {error}").as_str(),
            )?;
            failed_protocols.push(protocol);
            if protocol == &ServerProtocol::Ssh
                && fetch_or_list_error_is_not_authentication_failure(&error)
            {
                // authenticated by failed to complete request
                break;
            }
        } else {
            success = true;
            if !failed_protocols.is_empty() {
                term.write_line(format!("fetch: succeeded over {protocol}").as_str())?;
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
        term.write_line(format!("fetch: {error}").as_str())?;
        Err(error)
    }
}

#[allow(clippy::cast_precision_loss)]
#[allow(clippy::float_cmp)]
#[allow(clippy::needless_pass_by_value)]
fn report_on_transfer_progress(
    progress_stats: &Progress<'_>,
    start_time: &Instant,
    end_time: &Option<Instant>,
) -> Vec<String> {
    let mut report = vec![];
    let total = progress_stats.total_objects() as f64;
    if total == 0.0 {
        return report;
    }
    let received = progress_stats.received_objects() as f64;
    let percentage = ((received / total) * 100.0)
        // always round down because 100% complete is misleading when its not complete
        .floor();

    let received_bytes = progress_stats.received_bytes() as f64;

    let (size, unit) = if received_bytes >= (1024.0 * 1024.0) {
        (received_bytes / (1024.0 * 1024.0), "MiB")
    } else {
        (received_bytes / 1024.0, "KiB")
    };

    let speed = {
        let duration = if let Some(end_time) = end_time {
            (*end_time - *start_time).as_millis() as f64
        } else {
            start_time.elapsed().as_millis() as f64
        };

        if duration > 0.0 {
            (received_bytes / (1024.0 * 1024.0)) / (duration / 1000.0) // Convert bytes to MiB and milliseconds to seconds
        } else {
            0.0
        }
    };

    // Format the output for receiving objects
    report.push(format!(
        "Receiving objects: {percentage}% ({received}/{total}) {size:.2} {unit}  | {speed:.2} MiB/s{}",
        if received == total {
            ", done."
        } else { ""},
    ));
    if received == total {
        let indexed_deltas = progress_stats.indexed_deltas() as f64;
        let total_deltas = progress_stats.total_deltas() as f64;
        let percentage = ((indexed_deltas / total_deltas) * 100.0)
            // always round down because 100% complete is misleading when its not complete
            .floor();
        report.push(format!(
            "Resolving deltas: {percentage}% ({indexed_deltas}/{total_deltas}){}",
            if indexed_deltas == total_deltas {
                ", done."
            } else {
                ""
            },
        ));
    }
    report
}

struct FetchReporter<'a> {
    remote_msgs: Vec<String>,
    transfer_progress_msgs: Vec<String>,
    term: &'a console::Term,
    start_time: Option<Instant>,
    end_time: Option<Instant>,
}
impl<'a> FetchReporter<'a> {
    fn new(term: &'a console::Term) -> Self {
        Self {
            remote_msgs: vec![],
            transfer_progress_msgs: vec![],
            term,
            start_time: None,
            end_time: None,
        }
    }
    fn write_all(&self, lines_to_clear: usize) {
        let _ = self.term.clear_last_lines(lines_to_clear);
        for msg in &self.remote_msgs {
            let _ = self.term.write_line(msg);
        }
        for msg in &self.transfer_progress_msgs {
            let _ = self.term.write_line(msg);
        }
    }
    fn count_all_existing_lines(&self) -> usize {
        self.remote_msgs.len() + self.transfer_progress_msgs.len()
    }
    fn just_write_transfer_progress(&self, lines_to_clear: usize) {
        let _ = self.term.clear_last_lines(lines_to_clear);
        for msg in &self.transfer_progress_msgs {
            let _ = self.term.write_line(msg);
        }
    }
    fn just_count_transfer_progress(&self) -> usize {
        self.transfer_progress_msgs.len()
    }
    fn process_remote_msg(&mut self, data: &[u8]) {
        let existing_lines = self.count_all_existing_lines();
        if let Ok(data) = str::from_utf8(data) {
            let data = data
                .split(['\n', '\r'])
                .find(|line| !line.is_empty())
                .unwrap_or("")
                .trim();
            if !data.is_empty() {
                let msg = format!("remote: {data}");
                if let Some(last) = self.remote_msgs.last() {
                    if (last.contains('%') && !last.contains("100%"))
                        || last == &msg.replace(", done.", "")
                    {
                        self.remote_msgs.pop();
                    }
                }
                self.remote_msgs.push(msg);
                self.write_all(existing_lines);
            }
        }
    }
    fn process_transfer_progress_update(&mut self, progress_stats: &git2::Progress<'_>) {
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }
        let existing_lines = self.just_count_transfer_progress();
        let updated =
            report_on_transfer_progress(progress_stats, &self.start_time.unwrap(), &self.end_time);
        if self.transfer_progress_msgs.len() <= updated.len() {
            if self.end_time.is_none() && updated.first().is_some_and(|f| f.contains("100%")) {
                self.end_time = Some(Instant::now());
            }
            // once "Resolving Deltas" is complete, deltas get reset to 0 and it stops
            // reporting on it so we want to keep the old report
            self.transfer_progress_msgs = updated;
        }
        self.just_write_transfer_progress(existing_lines);
    }
}

fn fetch_from_git_server_url(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
    dont_authenticate: bool,
    term: &console::Term,
) -> Result<()> {
    if git_server_url.parse::<CloneUrl>()?.protocol() == ServerProtocol::Ssh && !check_ssh_keys() {
        bail!("no ssh keys found");
    }
    let git_config = git_repo.config()?;
    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    let auth = GitAuthenticator::default();
    let mut fetch_options = git2::FetchOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    let fetch_reporter = Arc::new(Mutex::new(FetchReporter::new(term)));
    remote_callbacks.sideband_progress({
        let fetch_reporter = Arc::clone(&fetch_reporter);
        move |data| {
            let mut reporter = fetch_reporter.lock().unwrap();
            reporter.process_remote_msg(data);
            true
        }
    });
    remote_callbacks.transfer_progress({
        let fetch_reporter = Arc::clone(&fetch_reporter);
        move |stats| {
            let mut reporter = fetch_reporter.lock().unwrap();
            reporter.process_transfer_progress_update(&stats);
            true
        }
    });

    if !dont_authenticate {
        remote_callbacks.credentials(auth.credentials(&git_config));
    }
    fetch_options.remote_callbacks(remote_callbacks);
    git_server_remote.download(oids, Some(&mut fetch_options))?;

    git_server_remote.disconnect()?;
    Ok(())
}