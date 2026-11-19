use core::str;
use std::{
    collections::HashMap,
    fmt, fs,
    io::{self, Stdin},
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use git2::Repository;
use nostr::prelude::{Event, EventId, Kind, PublicKey, nip19::ToBech32};

use crate::{
    client::{
        get_all_proposal_patch_pr_pr_update_events_from_cache, get_events_from_local_cache,
        get_proposals_and_revisions_from_cache,
    },
    git::{
        Repo, RepoActions,
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
    },
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, event_is_revision_root,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, get_status,
        is_event_proposal_root_for_branch, status_kinds,
    },
    repo_ref::{RepoRef, extract_npub, is_grasp_server_clone_url},
};

pub fn get_short_git_server_name(url: &str) -> std::string::String {
    // Check if this is a grasp server URL
    if is_grasp_server_clone_url(url) {
        // Try to extract and format the grasp server URL
        if let Ok(npub) = extract_npub(url) {
            // Parse the URL to get the domain and identifier
            if let Ok(clone_url) = CloneUrl::from_str(url) {
                let domain = clone_url.domain();
                let path = clone_url.short_name();

                // Extract the identifier from the path (after /{npub}/)
                if let Some(after_npub) = path.split(&format!("/{}/", npub)).nth(1) {
                    // Truncate npub to show first 10 and last 5 characters
                    let truncated_npub = if npub.len() > 20 {
                        format!("{}...{}", &npub[..4], &npub[npub.len() - 5..])
                    } else {
                        npub.to_string()
                    };

                    return format!("{}/{}/{}", domain, truncated_npub, after_npub);
                }
            }
        }
    }

    // Fall back to default behavior for non-grasp server URLs
    CloneUrl::from_str(url)
        .map(|u| u.short_name())
        .unwrap_or_else(|_| url.to_string())
}

pub fn get_remote_name_by_url(git_repo: &Repository, url: &str) -> Result<String> {
    let remotes = git_repo.remotes()?;
    remotes
        .iter()
        .filter_map(|r| r.ok().flatten())
        .find(|name| {
            git_repo
                .find_remote(name)
                .ok()
                .and_then(|remote| remote.url().ok().map(str::to_string))
                .is_some_and(|remote_url| url == remote_url)
        })
        .map(str::to_string)
        .context("could not find remote with matching url")
}

pub fn get_oids_from_fetch_batch(
    stdin: &Stdin,
    initial_oid: &str,
    initial_refstr: &str,
) -> Result<HashMap<String, String>> {
    let mut line = String::new();
    let mut batch = HashMap::new();
    batch.insert(initial_refstr.to_string(), initial_oid.to_string());
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["fetch", oid, refstr] => {
                batch.insert((*refstr).to_string(), (*oid).to_string());
            }
            [] => break,
            _ => bail!(
                "after a `fetch` command we are only expecting another fetch or an empty line"
            ),
        }
    }
    Ok(batch)
}

/// Read one line from stdin, and split it into tokens.
pub fn read_line<'a>(stdin: &io::Stdin, line: &'a mut String) -> io::Result<Vec<&'a str>> {
    line.clear();

    let read = stdin.read_line(line)?;
    if read == 0 {
        return Ok(vec![]);
    }
    let line = line.trim();
    let tokens = line.split(' ').filter(|t| !t.is_empty()).collect();

    Ok(tokens)
}

/// Returns the `KIND_PULL_REQUEST` revision-root event from a proposal's full
/// event set — present only when the original patch proposal was later upgraded
/// to a PR kind via a force-push or `--force-pr` send.  Returns `None` for
/// normal patch threads and for proposals that started as `KIND_PULL_REQUEST`.
///
/// This is the event that subsequent PR updates should reference via their `E`
/// tag, ensuring the tag always points at the PR kind event (never a PR update)
/// regardless of how many PR updates have occurred since the upgrade. CI
/// events anchor on the same event, so `ngit ci status` resolves a thread's
/// `#E` anchor through this rule rather than duplicating it.
#[must_use]
pub fn pr_upgrade_root(commits_events: &[Event]) -> Option<Event> {
    commits_events
        .iter()
        .filter(|e| event_is_revision_root(e) && e.kind.eq(&KIND_PULL_REQUEST))
        .max_by_key(|e| e.created_at)
        .cloned()
}

pub async fn get_open_or_draft_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<EventId, (Event, Vec<Event>, Option<Event>)>> {
    let git_repo_path = git_repo.workdir_or_git_dir();
    let proposals_and_revisions: Vec<nostr::prelude::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;
    let proposals: Vec<nostr::prelude::Event> = proposals_and_revisions
        .iter()
        .filter(|e|
            // If we wanted to treat to list Pull Requests that revise a Patch we would do this:
            // e.kind.eq(&KIND_PULL_REQUEST) ||
            !event_is_revision_root(e))
        .cloned()
        .collect();

    let statuses: Vec<nostr::prelude::Event> = {
        let mut statuses = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };
    let mut open_or_draft_proposals = HashMap::new();

    for proposal in &proposals {
        // Pass the unfiltered list (including PR revision-roots) so `get_status`
        // can recover the open/draft status of a patch that was closed because
        // it was upgraded to a PR (the PR revision-root carries the live status).
        let status = get_status(proposal, repo_ref, &statuses, &proposals_and_revisions);
        if [Kind::GitStatusOpen, Kind::GitStatusDraft].contains(&status) {
            if let Ok(commits_events) = get_all_proposal_patch_pr_pr_update_events_from_cache(
                git_repo_path,
                repo_ref,
                &proposal.id,
            )
            .await
            {
                if let Ok(most_recent_proposal_patch_chain) =
                    get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events.clone())
                {
                    open_or_draft_proposals.insert(
                        proposal.id,
                        (
                            proposal.clone(),
                            most_recent_proposal_patch_chain,
                            pr_upgrade_root(&commits_events),
                        ),
                    );
                }
            }
        }
    }
    Ok(open_or_draft_proposals)
}

pub async fn get_all_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<EventId, (Event, Vec<Event>, Option<Event>)>> {
    let git_repo_path = git_repo.workdir_or_git_dir();
    let proposals: Vec<nostr::prelude::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .filter(|e|
                // If we wanted to treat to list Pull Requests that revise a Patch we would do this:
                // e.kind.eq(&KIND_PULL_REQUEST) ||
                !event_is_revision_root(e))
            .cloned()
            .collect();

    let mut all_proposals = HashMap::new();

    for proposal in proposals {
        if let Ok(commits_events) = get_all_proposal_patch_pr_pr_update_events_from_cache(
            git_repo_path,
            repo_ref,
            &proposal.id,
        )
        .await
        {
            if let Ok(most_recent_proposal_patch_chain) =
                get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events.clone())
            {
                all_proposals.insert(
                    proposal.id,
                    (
                        proposal,
                        most_recent_proposal_patch_chain,
                        pr_upgrade_root(&commits_events),
                    ),
                );
            }
        }
    }
    Ok(all_proposals)
}

pub async fn proposal_tip_is_pr_or_pr_update(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    proposal_id: &EventId,
) -> Result<bool> {
    let commits_events =
        get_all_proposal_patch_pr_pr_update_events_from_cache(git_repo_path, repo_ref, proposal_id)
            .await
            .context(format!(
                "cannot get existing proposal events for {}",
                proposal_id.to_bech32()?
            ))?;
    let most_recent_proposal_patch_chain = get_pr_tip_event_or_most_recent_patch_with_ancestors(
        commits_events.clone(),
    )
    .context(format!(
        "cannot find tip from proposal events for {}",
        proposal_id.to_bech32()?,
    ))?;

    Ok([KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(
        &most_recent_proposal_patch_chain
            .first()
            .context(format!(
                "cannot find any proposal events for {}",
                proposal_id.to_bech32()?
            ))?
            .kind,
    ))
}

#[allow(clippy::type_complexity)]
pub fn find_proposal_and_patches_by_branch_name<'a>(
    refstr: &'a str,
    proposals: &'a HashMap<EventId, (Event, Vec<Event>, Option<Event>)>,
    current_user: Option<&PublicKey>,
) -> Option<(&'a EventId, &'a (Event, Vec<Event>, Option<Event>))> {
    proposals.iter().find(|(_, (proposal, _, _))| {
        is_event_proposal_root_for_branch(proposal, refstr, current_user).unwrap_or(false)
    })
}

pub fn join_with_and<T: ToString>(items: &[T]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        _ => {
            let last_item = items.last().unwrap().to_string();
            let rest = &items[..items.len() - 1];
            format!(
                "{} and {}",
                rest.iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                last_item
            )
        }
    }
}

/// get an ordered vector of server protocols to attempt
pub fn get_read_protocols_to_try(
    git_repo: &Repo,
    server_url: &CloneUrl,
    decoded_nostr_url: &NostrUrlDecoded,
    is_grasp_server: bool,
) -> Vec<ServerProtocol> {
    if is_grasp_server {
        if server_url.protocol() == ServerProtocol::Http {
            vec![(ServerProtocol::UnauthHttp)]
        } else {
            vec![(ServerProtocol::UnauthHttps)]
        }
    } else if server_url.protocol() == ServerProtocol::Filesystem {
        vec![(ServerProtocol::Filesystem)]
    } else if let Some(protocol) = &decoded_nostr_url.protocol {
        vec![protocol.clone()]
    } else {
        let mut list = if server_url.protocol() == ServerProtocol::Http {
            vec![
                ServerProtocol::UnauthHttp,
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Http,
            ]
        } else if server_url.protocol() == ServerProtocol::Ftp {
            vec![ServerProtocol::Ftp, ServerProtocol::Ssh]
        } else if server_url.protocol() == ServerProtocol::Git {
            vec![
                ServerProtocol::Git,
                ServerProtocol::UnauthHttps,
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Https,
            ]
        } else {
            vec![
                ServerProtocol::UnauthHttps,
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Https,
            ]
        };
        if let Some(protocol) = get_protocol_preference(git_repo, server_url, &Direction::Fetch) {
            if let Some(pos) = list.iter().position(|p| *p == protocol) {
                list.remove(pos);
                list.insert(0, protocol);
            }
        }
        list
    }
}

/// get an ordered vector of server protocols to attempt
pub fn get_write_protocols_to_try(
    git_repo: &Repo,
    server_url: &CloneUrl,
    decoded_nostr_url: &NostrUrlDecoded,
    is_grasp_server: bool,
) -> Vec<ServerProtocol> {
    if is_grasp_server {
        if server_url.protocol() == ServerProtocol::Http {
            vec![(ServerProtocol::UnauthHttp)]
        } else {
            vec![(ServerProtocol::UnauthHttps)]
        }
    } else if server_url.protocol() == ServerProtocol::Filesystem {
        vec![(ServerProtocol::Filesystem)]
    } else if let Some(protocol) = &decoded_nostr_url.protocol {
        vec![protocol.clone()]
    } else {
        let mut list = if server_url.protocol() == ServerProtocol::Http {
            vec![
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Http,
            ]
        } else if server_url.protocol() == ServerProtocol::Ftp {
            vec![ServerProtocol::Ssh, ServerProtocol::Ftp]
        } else if server_url.protocol() == ServerProtocol::Git {
            vec![
                ServerProtocol::Ssh,
                ServerProtocol::Git,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Https,
            ]
        } else {
            vec![
                ServerProtocol::Ssh,
                // note: list and fetch stop here if ssh was authenticated
                ServerProtocol::Https,
            ]
        };
        if let Some(protocol) = get_protocol_preference(git_repo, server_url, &Direction::Push) {
            if let Some(pos) = list.iter().position(|p| *p == protocol) {
                list.remove(pos);
                list.insert(0, protocol);
            }
        }

        list
    }
}

#[derive(Debug, PartialEq)]
pub enum Direction {
    Push,
    Fetch,
}
impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Direction::Push => write!(f, "push"),
            Direction::Fetch => write!(f, "fetch"),
        }
    }
}

pub fn get_protocol_preference(
    git_repo: &Repo,
    server_url: &CloneUrl,
    direction: &Direction,
) -> Option<ServerProtocol> {
    let server_short_name = server_url.short_name();
    if let Ok(Some(list)) =
        git_repo.get_git_config_item(format!("nostr.protocol-{direction}").as_str(), Some(false))
    {
        for item in list.split(';') {
            let pair = item.split(',').collect::<Vec<&str>>();
            if let Some(url) = pair.get(1) {
                if *url == server_short_name {
                    if let Some(protocol) = pair.first() {
                        if let Ok(protocol) = ServerProtocol::from_str(protocol) {
                            return Some(protocol);
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn set_protocol_preference(
    git_repo: &Repo,
    protocol: &ServerProtocol,
    server_url: &CloneUrl,
    direction: &Direction,
) -> Result<()> {
    let server_short_name = server_url.short_name();
    let mut new = String::new();
    if let Some(list) =
        git_repo.get_git_config_item(format!("nostr.protocol-{direction}").as_str(), Some(false))?
    {
        for item in list.split(';') {
            let pair = item.split(',').collect::<Vec<&str>>();
            if let Some(url) = pair.get(1) {
                if *url != server_short_name && !item.is_empty() {
                    new.push_str(format!("{item};").as_str());
                }
            }
        }
    }
    new.push_str(format!("{protocol},{server_short_name};").as_str());

    git_repo.save_git_config_item(
        format!("nostr.protocol-{direction}").as_str(),
        new.as_str(),
        false,
    )
}

/// Build a libgit2 `ProxyOptions` configured to route the given URL
/// through the local Tor SOCKS5 proxy when the host is `.onion`, or
/// returns `None` otherwise so callers fall back to libgit2's default
/// (no proxy / auto-detected from gitconfig).
///
/// HTTP(S) onion operations use system Git, which supports SOCKS5h.
/// The proxy address comes from the same `NGIT_TOR_PROXY` env var as the
/// nostr-sdk relay proxy ([`crate::client::tor_socks5_proxy_addr`]); set
/// it to `none` to disable.
pub fn onion_proxy_options_for_url(url: &str) -> Result<Option<git2::ProxyOptions<'static>>> {
    if !crate::git::nostr_url::host_is_onion(url) {
        return Ok(None);
    }
    crate::client::ensure_onion_url_reachable(url)?;
    let addr = crate::client::tor_socks5_proxy_addr()
        .context("Tor proxy became unavailable after reachability check")?;
    let mut opts = git2::ProxyOptions::new();
    // `socks5h://` so DNS resolution happens at the proxy (mandatory for
    // `.onion` addresses, which the local resolver can't handle).
    opts.url(&format!("socks5h://{addr}"));
    Ok(Some(opts))
}

/// Whether an error is a libgit2 internal assertion failure. libgit2 only
/// produces "unrecoverable internal error: '...'" (`assert_safe.h`) from
/// failed `GIT_ASSERT` checks, so this signals a libgit2 bug rather than a
/// transport, server or authentication failure, and the operation may be
/// worth retrying with the system git binary. For example, libgit2 1.9
/// asserts while parsing a ref advertisement whose last capability is
/// `object-format=...`, which tangled.org sends. Matched on the message
/// because the git2 crate does not expose libgit2's `GIT_ERROR_INTERNAL`
/// error class.
pub fn is_libgit2_internal_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<git2::Error>()
        .is_some_and(|error| error.message().contains("unrecoverable internal error"))
}

pub fn error_might_be_authentication_related(error: &anyhow::Error) -> bool {
    let error_str = error.to_string();
    for s in [
        "no ssh keys found",
        "invalid or unknown remote ssh hostkey",
        "authentication",
        "Permission",
        "permission",
        "not found",
    ] {
        if error_str.contains(s) {
            return true;
        }
    }
    false
}

/// Resolve a repository-relative path, confining it to the repository root.
///
/// This function:
/// - Rejects absolute paths (unless `allow_absolute` is true)
/// - Rejects paths with `..` components that would escape the repository
/// - Canonicalizes the path and ensures it stays within the canonical
///   repository root
/// - Rejects symlinks that resolve outside the repository root
///
/// This is the single shared helper for all manifest-supplied file paths used
/// during publication (`assets[].file`, `release_source`, release notes files,
/// application media). User-supplied CLI arguments may allow absolute paths as
/// a deliberate read primitive. Such explicit paths retain their former
/// resolution behavior, including parent components.
pub fn resolve_repo_relative_path(
    repository_root: &Path,
    declared: &Path,
    allow_absolute: bool,
) -> Result<PathBuf> {
    // Explicit CLI inputs retain their original semantics: absolute paths pass
    // through and relative paths (including .. components) resolve from the
    // repository root. Only manifest content is confined to the repository.
    if allow_absolute {
        return Ok(if declared.is_absolute() {
            declared.to_path_buf()
        } else {
            repository_root.join(declared)
        });
    }

    // Reject absolute paths from repository manifests.
    if declared.is_absolute() {
        bail!(
            "path must be repository-relative, not absolute: {}",
            declared.display()
        );
    }

    // Build a relative path while rejecting ParentDir and RootDir components
    let mut relative = PathBuf::new();
    for component in declared.components() {
        match component {
            Component::Normal(component) => relative.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "path must remain within the repository: {}",
                    declared.display()
                );
            }
        }
    }

    // Ensure the relative path is not empty
    if relative.as_os_str().is_empty() {
        bail!("path must name a file");
    }

    // Canonicalize the repository root
    let root =
        fs::canonicalize(repository_root).context("failed to resolve the repository root")?;

    // Construct the full source path
    let source = root.join(&relative);

    // Canonicalize the source path (follows symlinks)
    let resolved = fs::canonicalize(&source)
        .with_context(|| format!("failed to resolve path {}", declared.display()))?;

    // Ensure the resolved path is within the repository root
    if !resolved.starts_with(&root) {
        bail!(
            "path must not resolve outside the repository: {}",
            declared.display()
        );
    }

    Ok(source)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    mod resolve_repo_relative_path {
        use super::*;

        #[test]
        fn accepts_legitimate_nested_relative_path() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();
            let file_path = repo_root.join("dist/app.apk");
            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            fs::write(&file_path, b"content").unwrap();

            let result =
                resolve_repo_relative_path(&repo_root, Path::new("dist/app.apk"), false).unwrap();
            assert_eq!(result, file_path);
        }

        #[test]
        fn rejects_absolute_path() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let result = resolve_repo_relative_path(&repo_root, Path::new("/etc/passwd"), false);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must be repository-relative")
            );
        }

        #[test]
        fn accepts_absolute_path_when_allowed() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();
            let absolute_file = root.path().join("outside.txt");
            fs::write(&absolute_file, b"content").unwrap();

            let result = resolve_repo_relative_path(&repo_root, &absolute_file, true).unwrap();
            assert_eq!(result, absolute_file);
        }

        #[test]
        fn explicit_cli_paths_keep_parent_component_semantics() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let result =
                resolve_repo_relative_path(&repo_root, Path::new("../notes.txt"), true).unwrap();
            assert_eq!(result, repo_root.join("../notes.txt"));
        }

        #[test]
        fn rejects_parent_dir_escape() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let result =
                resolve_repo_relative_path(&repo_root, Path::new("../.ssh/id_ed25519"), false);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must remain within the repository")
            );
        }

        #[test]
        fn rejects_root_dir_escape() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let result = resolve_repo_relative_path(&repo_root, Path::new("/etc/passwd"), false);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must be repository-relative")
            );
        }

        #[cfg(unix)]
        #[test]
        fn rejects_symlink_outside_repo() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            // Create a file outside the repo
            let outside_file = root.path().join("secret.txt");
            fs::write(&outside_file, b"secret").unwrap();

            // Create a symlink inside the repo pointing outside
            let symlink = repo_root.join("link.txt");
            std::os::unix::fs::symlink(&outside_file, &symlink).unwrap();

            let result = resolve_repo_relative_path(&repo_root, Path::new("link.txt"), false);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must not resolve outside the repository")
            );
        }

        #[cfg(unix)]
        #[test]
        fn accepts_symlink_inside_repo() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            // Create a file inside the repo
            let inside_file = repo_root.join("real.txt");
            fs::write(&inside_file, b"real").unwrap();

            // Create a symlink inside the repo pointing to the file
            let symlink = repo_root.join("link.txt");
            std::os::unix::fs::symlink(&inside_file, &symlink).unwrap();

            // The function returns the source path (the symlink), not the canonicalized
            // target
            let result =
                resolve_repo_relative_path(&repo_root, Path::new("link.txt"), false).unwrap();
            assert_eq!(result, symlink);
        }

        #[test]
        fn rejects_empty_path() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let result = resolve_repo_relative_path(&repo_root, Path::new(""), false);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("must name a file"));
        }

        #[test]
        fn accepts_curdir_component() {
            let root = tempdir().unwrap();
            let repo_root = root.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();
            let file_path = repo_root.join("dist/app.apk");
            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            fs::write(&file_path, b"content").unwrap();

            let result =
                resolve_repo_relative_path(&repo_root, Path::new("./dist/app.apk"), false).unwrap();
            assert_eq!(result, file_path);
        }
    }
    mod is_libgit2_internal_error {
        use super::*;

        fn git2_error(message: &str) -> anyhow::Error {
            git2::Error::new(
                git2::ErrorCode::GenericError,
                git2::ErrorClass::None,
                message,
            )
            .into()
        }

        #[test]
        fn detects_libgit2_assertion_failures() {
            assert!(is_libgit2_internal_error(&git2_error(
                "unrecoverable internal error: 'eos'"
            )));
        }

        #[test]
        fn detects_libgit2_assertion_failures_wrapped_with_context() {
            let error =
                git2_error("unrecoverable internal error: 'eos'").context("pushing to git server");
            assert!(is_libgit2_internal_error(&error));
        }

        #[test]
        fn ignores_other_git2_errors() {
            assert!(!is_libgit2_internal_error(&git2_error(
                "failed to connect to example.com: Connection refused"
            )));
        }

        #[test]
        fn ignores_non_git2_errors() {
            assert!(!is_libgit2_internal_error(&anyhow::anyhow!(
                "unrecoverable internal error: 'eos'"
            )));
        }
    }

    mod join_with_and {
        use super::*;
        #[test]
        fn test_empty() {
            let items: Vec<&str> = vec![];
            assert_eq!(join_with_and(&items), "");
        }

        #[test]
        fn test_single_item() {
            let items = vec!["apple"];
            assert_eq!(join_with_and(&items), "apple");
        }

        #[test]
        fn test_two_items() {
            let items = vec!["apple", "banana"];
            assert_eq!(join_with_and(&items), "apple and banana");
        }

        #[test]
        fn test_three_items() {
            let items = vec!["apple", "banana", "cherry"];
            assert_eq!(join_with_and(&items), "apple, banana and cherry");
        }

        #[test]
        fn test_four_items() {
            let items = vec!["apple", "banana", "cherry", "date"];
            assert_eq!(join_with_and(&items), "apple, banana, cherry and date");
        }

        #[test]
        fn test_multiple_items() {
            let items = vec!["one", "two", "three", "four", "five"];
            assert_eq!(join_with_and(&items), "one, two, three, four and five");
        }
    }
}
