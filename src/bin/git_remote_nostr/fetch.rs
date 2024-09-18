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
use nostr::nips::nip19;
use nostr_sdk::ToBech32;

use crate::utils::{
    count_lines_per_msg_vec, fetch_or_list_error_is_not_authentication_failure,
    find_proposal_and_patches_by_branch_name, get_oids_from_fetch_batch, get_open_proposals,
    get_read_protocols_to_try, join_with_and, set_protocol_preference, Direction,
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
            git_repo,
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
                            if let Err(error) = git_repo.create_commit_from_patch(patch) {
                                term.write_line(
                                    format!(
                                        "WARNING: cannot create branch for {refstr}, error: {error} for patch {}",
                                        nip19::Nip19Event {
                                            event_id: patch.id(),
                                            author: Some(patch.author()),
                                            kind: Some(patch.kind()),
                                            relays: if let Some(relay) = repo_ref.relays.first() {
                                                vec![relay.to_string()]
                                            } else { vec![]},
                                        }.to_bech32().unwrap_or_default()
                                    )
                                    .as_str(),
                                )?;
                                break;
                            }
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
    git_repo: &Repo,
    oids: &[String],
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    term: &console::Term,
) -> Result<()> {
    let server_url = git_server_url.parse::<CloneUrl>()?;

    let protocols_to_attempt = get_read_protocols_to_try(git_repo, &server_url, decoded_nostr_url);

    let mut failed_protocols = vec![];
    let mut success = false;
    for protocol in &protocols_to_attempt {
        term.write_line(
            format!("fetching {} over {protocol}...", server_url.short_name(),).as_str(),
        )?;

        let formatted_url = server_url.format_as(protocol, &decoded_nostr_url.user)?;
        let res = fetch_from_git_server_url(
            &git_repo.git_repo,
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
        if total_deltas > 0.0 {
            report.push(format!(
                "Resolving deltas: {percentage}% ({indexed_deltas}/{total_deltas}){}",
                if indexed_deltas == total_deltas {
                    ", done."
                } else {
                    ""
                },
            ));
        }
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
            let _ = self.term.write_line(format!("remote: {msg}").as_str());
        }
        for msg in &self.transfer_progress_msgs {
            let _ = self.term.write_line(msg);
        }
    }
    fn count_all_existing_lines(&self) -> usize {
        let width = self.term.size().1;
        count_lines_per_msg_vec(width, &self.remote_msgs, "remote: ".len())
            + count_lines_per_msg_vec(width, &self.transfer_progress_msgs, 0)
    }
    fn just_write_transfer_progress(&self, lines_to_clear: usize) {
        let _ = self.term.clear_last_lines(lines_to_clear);
        for msg in &self.transfer_progress_msgs {
            let _ = self.term.write_line(msg);
        }
    }
    fn just_count_transfer_progress(&self) -> usize {
        let width = self.term.size().1;
        count_lines_per_msg_vec(width, &self.transfer_progress_msgs, 0)
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
                    // if previous line begins with x but doesnt finish with y then its part of the
                    // same msg
                    if (last.starts_with("Enume") && !last.ends_with(", done."))
                        || ((last.starts_with("Compre") || last.starts_with("Count"))
                            && !last.contains(')'))
                    {
                        let last = self.remote_msgs.pop().unwrap();
                        self.remote_msgs.push(format!("{last}{msg}"));
                    // if previous msg contains % and its not 100% then it
                    // should be overwritten
                    } else if (last.contains('%') && !last.contains("100%"))
                        // but also if the next message is identical with "", done." appended
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

#[cfg(test)]
mod tests {

    use super::*;

    fn pass_through_fetch_reporter_proces_remote_msg(msgs: Vec<&str>) -> Vec<String> {
        let term = console::Term::stdout();
        let mut reporter = FetchReporter::new(&term);
        for msg in msgs {
            reporter.process_remote_msg(msg.as_bytes());
        }
        reporter.remote_msgs
    }

    #[test]
    fn logs_single_msg() {
        assert_eq!(
            pass_through_fetch_reporter_proces_remote_msg(vec![
                "Enumerating objects: 23716, done.",
            ]),
            vec!["Enumerating objects: 23716, done."]
        );
    }

    #[test]
    fn logs_multiple_msgs() {
        assert_eq!(
            pass_through_fetch_reporter_proces_remote_msg(vec![
                "Enumerating objects: 23716, done.",
                "Counting objects:   0% (1/2195)",
            ]),
            vec![
                "Enumerating objects: 23716, done.",
                "Counting objects:   0% (1/2195)",
            ]
        );
    }

    mod ignores {
        use super::*;

        #[test]
        fn empty_msgs() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerating objects: 23716, done.",
                    "",
                    "Counting objects:   0% (1/2195)",
                    "",
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                ]
            );
        }

        #[test]
        fn whitespace_msgs() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerating objects: 23716, done.",
                    "   ",
                    "Counting objects:   0% (1/2195)",
                    "  \r\n  \r",
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                ]
            );
        }
    }

    mod splits {
        use super::*;

        #[test]
        fn multiple_lines_in_single_msg() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerating objects: 23716, done.\r\nCounting objects:   0% (1/2195)",
                    "",
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                ]
            );
        }
    }

    mod joins_lines_sent_over_multiple_msgs {
        use super::*;

        #[test]
        fn enumerating() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerat",
                    "ing objec",
                    "ts: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                ]
            );
        }
        #[test]
        fn counting() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerating objects: 23716, done.",
                    "Counting obj",
                    "ects:   0% (1/2195)",
                    "Count",
                    "ing objects:   1% (22/",
                    "2195)",
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   1% (22/2195)",
                ]
            );
        }
        #[test]
        fn compressing() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Compress",
                    "ing obj",
                    "ect",
                    "s:   0% (1/56",
                    "0)"
                ]),
                vec!["Compressing objects:   0% (1/560)"]
            );
        }
    }

    #[test]
    fn msgs_with_pc_and_not_100pc_are_replaced() {
        assert_eq!(
            pass_through_fetch_reporter_proces_remote_msg(vec![
                "Enumerating objects: 23716, done.",
                "Counting objects:   0% (1/2195)",
                "Counting objects:   1% (22/2195)",
            ]),
            vec![
                "Enumerating objects: 23716, done.",
                "Counting objects:   1% (22/2195)",
            ]
        );
    }
    mod msgs_with_pc_100pc_are_not_replaced {
        use super::*;

        #[test]
        fn when_next_msg_is_not_identical_but_with_done() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                    "Counting objects:   1% (22/2195)",
                    "Counting objects: 100% (2195/2195)",
                    "Compressing objects:   0% (1/560)"
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects: 100% (2195/2195)",
                    "Compressing objects:   0% (1/560)"
                ]
            );
        }

        #[test]
        fn but_is_when_next_msg_is_identical_but_with_done_appended() {
            assert_eq!(
                pass_through_fetch_reporter_proces_remote_msg(vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects:   0% (1/2195)",
                    "Counting objects:   1% (22/2195)",
                    "Counting objects: 100% (2195/2195)",
                    "Counting objects: 100% (2195/2195), done.",
                ]),
                vec![
                    "Enumerating objects: 23716, done.",
                    "Counting objects: 100% (2195/2195), done.",
                ]
            );
        }
    }
}
