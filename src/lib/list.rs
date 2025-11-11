use std::{collections::HashMap, path::PathBuf, str::FromStr};

use anyhow::{Result, anyhow};
use auth_git2::GitAuthenticator;
use nostr::hashes::sha1::Hash as Sha1Hash;

use crate::{
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
    },
    repo_ref::is_grasp_server_clone_url,
    repo_state::RepoState,
    utils::{
        Direction, get_read_protocols_to_try, get_short_git_server_name, join_with_and,
        set_protocol_preference,
    },
};

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

pub fn list_from_remotes(
    term: &console::Term,
    git_repo: &Repo,
    git_servers: &Vec<String>,
    decoded_nostr_url: &NostrUrlDecoded,
) -> HashMap<String, (HashMap<String, String>, bool)> {
    let mut remote_states = HashMap::new();
    let mut errors = HashMap::new();
    for url in git_servers {
        let is_grasp_server = is_grasp_server_clone_url(url);
        match list_from_remote(term, git_repo, url, decoded_nostr_url, is_grasp_server) {
            Err(error) => {
                errors.insert(url, error);
            }
            Ok(state) => {
                remote_states.insert(url.to_string(), (state, is_grasp_server));
            }
        }
    }
    remote_states
}

pub fn list_from_remote(
    term: &console::Term,
    git_repo: &Repo,
    git_server_url: &str,
    decoded_nostr_url: &NostrUrlDecoded,
    is_grasp_server: bool,
) -> Result<HashMap<String, String>> {
    let server_url = git_server_url.parse::<CloneUrl>()?;
    let protocols_to_attempt =
        get_read_protocols_to_try(git_repo, &server_url, decoded_nostr_url, is_grasp_server);

    let mut failed_protocols = vec![];
    let mut remote_state: Option<HashMap<String, String>> = None;

    for protocol in &protocols_to_attempt {
        term.write_line(
            format!(
                "fetching {} ref list over {protocol}...",
                server_url.short_name(),
            )
            .as_str(),
        )?;

        let formatted_url = server_url.format_as(protocol)?;

        let res = list_from_remote_url(
            git_repo,
            &formatted_url,
            decoded_nostr_url.ssh_key_file_path().as_ref(),
            [ServerProtocol::UnauthHttps, ServerProtocol::UnauthHttp].contains(protocol),
            term,
        );

        match res {
            Ok(state) => {
                remote_state = Some(state);
                if !is_grasp_server && !failed_protocols.is_empty() {
                    term.write_line(
                        format!(
                            "list: succeeded over {protocol} from {}",
                            server_url.short_name(),
                        )
                        .as_str(),
                    )?;
                    let _ =
                        set_protocol_preference(git_repo, protocol, &server_url, &Direction::Fetch);
                }
                break;
            }
            Err(error) => {
                if is_grasp_server {
                    term.write_line(&format!("list: failed: {error}"))?;
                } else {
                    term.write_line(&format!(
                        "list: {formatted_url} failed over {protocol}{}: {error}",
                        if protocol == &ServerProtocol::Ssh {
                            if let Some(ssh_key_file) = &decoded_nostr_url.ssh_key_file_path() {
                                format!(" with ssh key from {ssh_key_file}")
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    ))?;
                }
                failed_protocols.push(protocol);
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
        if !is_grasp_server {
            term.write_line(format!("list: {error}").as_str())?;
        }
        Err(error)
    }
}

fn list_from_remote_url(
    git_repo: &Repo,
    git_server_remote_url: &str,
    ssh_key_file: Option<&String>,
    dont_authenticate: bool,
    term: &console::Term,
) -> Result<HashMap<String, String>> {
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
    term.write_line("list: connecting...")?;
    git_server_remote.connect_auth(git2::Direction::Fetch, Some(remote_callbacks), None)?;
    term.clear_last_lines(1)?;
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
        for (url, (remote_state, _is_grasp_server)) in remote_states {
            let remote_name = get_short_git_server_name(git_repo, url);
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
    git_repo: &Repo,
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
            .find(|(url, _)| &get_short_git_server_name(git_repo, url) == remote_name)
            .map(|(_, (state, _))| state);

        if let Some(state) = remote_state {
            // Check if remote is completely empty
            if state.is_empty() {
                warnings.push(format!("WARNING: {remote_name} has no data."));
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
    use super::*;

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
