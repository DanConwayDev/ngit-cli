//! Repository-managed coding-agent guidance.
//!
//! The state file is deliberately separate from the files it verifies: storing
//! hashes in a managed file would make the hash self-referential.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use console::{Color, Style};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SKILL_PATH: &str = ".agents/skills/ngit/SKILL.md";
pub const CLAUDE_SKILL_PATH: &str = ".claude/skills/ngit/SKILL.md";
pub const AGENTS_PATH: &str = "AGENTS.md";
pub const CLAUDE_PATH: &str = "CLAUDE.md";
pub const STATE_PATH: &str = ".agents/ngit-guidance.json";
const START: &str = "<!-- ngit-agent-guidance:start -->";
const END: &str = "<!-- ngit-agent-guidance:end -->";
const CLAUDE_START: &str = "<!-- ngit-agent-guidance-claude:start -->";
const CLAUDE_END: &str = "<!-- ngit-agent-guidance-claude:end -->";
pub const WARNING_INTERVAL_SECS: i64 = 24 * 60 * 60;
const WARNING_SAMPLE_DAYS: u8 = 5;

const CANONICAL_SKILL: &str = include_str!("../../skills/ngit/SKILL.md");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedState {
    pub version: String,
    pub files: Vec<ManagedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuidanceStatus {
    pub installed: bool,
    pub installed_version: Option<String>,
    pub bundled_version: String,
    pub update_available: bool,
    pub modified_files: Vec<String>,
    pub managed_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningKind {
    Setup,
    Update,
}

#[must_use]
pub fn bundled_skill() -> &'static str {
    CANONICAL_SKILL
}

pub fn bundled_version() -> Result<String> {
    CANONICAL_SKILL
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("version:").map(str::trim))
        .map(|value| value.trim_matches('"').to_string())
        .filter(|value| !value.is_empty())
        .context("bundled ngit skill is missing metadata.version")
}

fn hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn managed_content(path: &str, content: &str) -> Option<String> {
    let (start, end) = match path {
        AGENTS_PATH => (START, END),
        CLAUDE_PATH => (CLAUDE_START, CLAUDE_END),
        _ => return Some(content.to_string()),
    };
    let from = content.find(start)?;
    let to = content[from..].find(end)? + from + end.len();
    Some(content[from..to].to_string())
}

fn policy() -> &'static str {
    "## Repository collaboration\n\nThis repository uses ngit and Nostr for issues, pull requests, reviews, comments, merges, and repository state.\n\n- Do not use GitHub or GitLab issues, pull requests, APIs, or the `gh`/`glab` CLIs for repository collaboration.\n- For issue, pull request, review, comment, merge, push, clone, or repository metadata tasks, load and follow `.agents/skills/ngit/SKILL.md`.\n- Treat a `nostr://` remote as authoritative.\n- Pull request branches must use the `pr/` prefix.\n- If ngit is unavailable, report that it is required rather than falling back to GitHub or GitLab."
}

fn managed_section(start: &str, end: &str, body: &str) -> String {
    format!("{start}\n{body}\n{end}\n")
}

fn replace_section(existing: Option<&str>, start: &str, end: &str, body: &str) -> Result<String> {
    let section = managed_section(start, end, body);
    let Some(existing) = existing else {
        return Ok(section);
    };
    let starts = existing
        .match_indices(start)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let ends = existing
        .match_indices(end)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(if existing.is_empty() {
            section
        } else if existing.ends_with('\n') {
            format!("{existing}\n{section}")
        } else {
            format!("{existing}\n\n{section}")
        }),
        ([from], [end_from]) if end_from >= from => {
            let after = *end_from + end.len();
            Ok(format!(
                "{}{}{}",
                &existing[..*from],
                section.trim_end(),
                &existing[after..]
            ))
        }
        _ => bail!("managed ngit agent guidance markers in file are malformed"),
    }
}

fn expected_files(root: &Path) -> Result<Vec<(String, String)>> {
    let agents = fs::read_to_string(root.join(AGENTS_PATH)).ok();
    let claude = fs::read_to_string(root.join(CLAUDE_PATH)).ok();
    Ok(vec![
        (SKILL_PATH.to_string(), CANONICAL_SKILL.to_string()),
        (CLAUDE_SKILL_PATH.to_string(), CANONICAL_SKILL.to_string()),
        (
            AGENTS_PATH.to_string(),
            replace_section(agents.as_deref(), START, END, policy())?,
        ),
        (
            CLAUDE_PATH.to_string(),
            replace_section(claude.as_deref(), CLAUDE_START, CLAUDE_END, "@AGENTS.md")?,
        ),
    ])
}

pub fn load_state(root: &Path) -> Result<Option<ManagedState>> {
    let path = root.join(STATE_PATH);
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_str(
        &fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .context("failed to parse ngit agent guidance state")
    .map(Some)
}

pub fn status(root: &Path) -> Result<GuidanceStatus> {
    let bundled_version = bundled_version()?;
    let Some(state) = load_state(root)? else {
        return Ok(GuidanceStatus {
            installed: false,
            installed_version: None,
            bundled_version,
            update_available: false,
            modified_files: vec![],
            managed_files: vec![
                SKILL_PATH.into(),
                CLAUDE_SKILL_PATH.into(),
                AGENTS_PATH.into(),
                CLAUDE_PATH.into(),
            ],
        });
    };
    let modified_files = state
        .files
        .iter()
        .filter(|file| {
            fs::read_to_string(root.join(&file.path))
                .ok()
                .and_then(|content| managed_content(&file.path, &content))
                .filter(|content| hash(content) == file.sha256)
                .is_none()
        })
        .map(|file| file.path.clone())
        .collect();
    Ok(GuidanceStatus {
        installed: true,
        update_available: version_is_newer(&state.version, &bundled_version),
        installed_version: Some(state.version),
        bundled_version,
        modified_files,
        managed_files: state.files.into_iter().map(|file| file.path).collect(),
    })
}

pub fn setup(root: &Path, force: bool) -> Result<GuidanceStatus> {
    write_guidance(root, force)?;
    status(root)
}

pub fn update(root: &Path, force: bool) -> Result<GuidanceStatus> {
    write_guidance(root, force).and_then(|_| status(root))
}

fn write_guidance(root: &Path, force: bool) -> Result<()> {
    if let Some(state) = load_state(root)? {
        let bundled = bundled_version()?;
        if !force && version_is_newer(&bundled, &state.version) {
            bail!(
                "refusing to downgrade ngit agent guidance from {} to {}; rerun with --force to override",
                state.version,
                bundled
            );
        }
        if !force {
            if let Some(path) = [SKILL_PATH, CLAUDE_SKILL_PATH, AGENTS_PATH, CLAUDE_PATH]
                .into_iter()
                .find(|path| {
                    !state.files.iter().any(|file| file.path == *path) && root.join(path).exists()
                })
            {
                bail!(
                    "refusing to overwrite unmanaged file `{path}`; rerun with --force to install ngit agent guidance"
                );
            }
            let current = status(root)?;
            if let Some(path) = current.modified_files.first() {
                bail!(
                    "refusing to overwrite locally modified managed file `{path}`; run `ngit agent update --diff`"
                );
            }
        }
    } else if !force {
        let existing = [SKILL_PATH, CLAUDE_SKILL_PATH]
            .into_iter()
            .find(|path| root.join(path).exists());
        if let Some(path) = existing {
            bail!(
                "refusing to overwrite unmanaged file `{path}`; rerun with --force to install ngit agent guidance"
            );
        }
    }
    let files = expected_files(root)?;
    for (relative, content) in &files {
        let path = root.join(relative);
        if fs::read_to_string(&path).ok().as_deref() != Some(content) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    let state = ManagedState {
        version: bundled_version()?,
        files: files
            .into_iter()
            .map(|(path, content)| ManagedFile {
                sha256: hash(&managed_content(&path, &content).expect("generated managed content")),
                path,
            })
            .collect(),
    };
    let state_path = root.join(STATE_PATH);
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        state_path,
        format!("{}\n", serde_json::to_string_pretty(&state)?),
    )?;
    Ok(())
}

pub fn proposed_diff(root: &Path) -> Result<String> {
    let mut output = String::new();
    let files = expected_files(root)?;
    let state = ManagedState {
        version: bundled_version()?,
        files: files
            .iter()
            .map(|(path, content)| ManagedFile {
                path: path.clone(),
                sha256: hash(&managed_content(path, content).expect("generated managed content")),
            })
            .collect(),
    };
    for (relative, expected) in files.into_iter().chain(std::iter::once((
        STATE_PATH.into(),
        format!("{}\n", serde_json::to_string_pretty(&state)?),
    ))) {
        let actual = fs::read_to_string(root.join(&relative)).unwrap_or_default();
        if actual != expected {
            output.push_str(&format!("--- a/{relative}\n+++ b/{relative}\n"));
            output.push_str(&simple_diff(&actual, &expected));
        }
    }
    Ok(output)
}

fn simple_diff(old: &str, new: &str) -> String {
    let mut result = String::new();
    for line in old.lines() {
        result.push('-');
        result.push_str(line);
        result.push('\n');
    }
    for line in new.lines() {
        result.push('+');
        result.push_str(line);
        result.push('\n');
    }
    result
}

#[must_use]
pub fn version_is_newer(installed: &str, bundled: &str) -> bool {
    fn parts(value: &str) -> Option<Vec<u64>> {
        value
            .split('.')
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    }
    match (parts(installed), parts(bundled)) {
        (Some(mut old), Some(mut new)) => {
            old.resize(3, 0);
            new.resize(3, 0);
            new > old
        }
        _ => false,
    }
}

#[must_use]
pub fn warning_for(status: &GuidanceStatus) -> Option<WarningKind> {
    if !status.installed {
        Some(WarningKind::Setup)
    } else if status.update_available {
        Some(WarningKind::Update)
    } else {
        None
    }
}

#[must_use]
pub fn warning_message(status: &GuidanceStatus) -> Option<String> {
    match warning_for(status)? {
        WarningKind::Setup => Some(
            "warning: this ngit repository has no agent guidance; run `ngit agent setup`".into(),
        ),
        WarningKind::Update => Some(format!(
            "warning: newer ngit agent guidance is available ({} -> {}); run `ngit agent update`",
            status.installed_version.as_deref().unwrap_or("unknown"),
            status.bundled_version
        )),
    }
}

#[must_use]
pub fn should_warn(last_seen: Option<i64>, now: i64) -> bool {
    last_seen.is_none_or(|last| now.saturating_sub(last) >= WARNING_INTERVAL_SECS)
}

#[must_use]
pub fn should_sample_warning(seed: &str, now: i64) -> bool {
    let digest = Sha256::digest(seed.as_bytes());
    let phase = i64::from(digest[0] % WARNING_SAMPLE_DAYS);
    let day = now.div_euclid(WARNING_INTERVAL_SECS);
    day.rem_euclid(i64::from(WARNING_SAMPLE_DAYS)) == phase
}

#[must_use]
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn paths_for_commit(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(expected_files(root)?
        .into_iter()
        .map(|(path, _)| root.join(path))
        .chain(std::iter::once(root.join(STATE_PATH)))
        .collect())
}

const COMMIT_MESSAGE: &str = "chore: update ngit agent guidance";

fn target_paths() -> [&'static str; 5] {
    [
        AGENTS_PATH,
        CLAUDE_PATH,
        SKILL_PATH,
        CLAUDE_SKILL_PATH,
        STATE_PATH,
    ]
}

/// Validate that a guidance-only commit can be made without absorbing any
/// unrelated repository changes. This deliberately happens before setup writes
/// its files, so callers can safely treat a failure as a no-op.
pub fn preflight_dedicated_commit(repo: &crate::git::Repo, root: &Path) -> Result<()> {
    // Parse the target files now: malformed markers are unsafe to replace, and
    // this also validates that desired content can be computed before writing.
    let _ = expected_files(root)?;
    let head = repo
        .git_repo
        .head()
        .context("cannot create a dedicated guidance commit without HEAD")?;
    if !head.is_branch() || repo.git_repo.head_detached()? {
        bail!("cannot create a guidance commit while HEAD is detached");
    }
    let parent = head.peel_to_commit()?;
    if repo.git_repo.state() != git2::RepositoryState::Clean {
        bail!("cannot create a guidance commit while a Git operation is in progress");
    }
    let mut index = repo.git_repo.index()?;
    if index.has_conflicts() || index.write_tree()? != parent.tree_id() {
        bail!("cannot create a guidance commit while the Git index contains changes");
    }
    for relative in target_paths() {
        let status = match repo.git_repo.status_file(Path::new(relative)) {
            Ok(status) => status,
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                // A target that is absent from HEAD, the index, and the
                // worktree is the normal first-install case.
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect guidance target `{relative}`"));
            }
        };
        if status.intersects(
            git2::Status::WT_NEW
                | git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED
                | git2::Status::WT_RENAMED
                | git2::Status::WT_TYPECHANGE
                | git2::Status::CONFLICTED,
        ) {
            bail!(
                "cannot create a guidance commit while target file `{relative}` has unstaged changes"
            );
        }
    }
    Ok(())
}

/// Create a commit containing only the installed guidance files. The caller
/// must run [`preflight_dedicated_commit`] before changing the worktree.
pub fn commit_guidance(repo: &crate::git::Repo, root: &Path) -> Result<bool> {
    let index_path = repo.git_repo.path().join("index");
    let original_index = if index_path.exists() {
        Some(
            fs::read(&index_path)
                .with_context(|| format!("failed to read {}", index_path.display()))?,
        )
    } else {
        None
    };
    let result = commit_guidance_inner(repo, root);
    if let Err(error) = result {
        match original_index {
            Some(contents) => fs::write(&index_path, contents).map(|_| ()),
            None if index_path.exists() => fs::remove_file(&index_path),
            None => Ok(()),
        }
        .with_context(|| format!("failed to restore {}", index_path.display()))?;
        return Err(error);
    }
    result
}

fn commit_guidance_inner(repo: &crate::git::Repo, root: &Path) -> Result<bool> {
    let head = repo
        .git_repo
        .head()
        .context("cannot create a dedicated guidance commit without HEAD")?;
    let parent = head.peel_to_commit()?;
    let tree = parent.tree()?;
    let mut index = repo.git_repo.index()?;
    index.read_tree(&tree)?;
    for path in paths_for_commit(root)? {
        let relative = path
            .strip_prefix(root)
            .context("guidance path outside worktree")?;
        index.add_path(relative)?;
    }
    let tree_id = index.write_tree_to(&repo.git_repo)?;
    if tree_id == tree.id() {
        return Ok(false);
    }
    let signature = repo
        .git_repo
        .signature()
        .context("cannot determine Git author for guidance commit")?;
    // Write the real index before moving HEAD. This means a successful commit
    // cannot subsequently fail while trying to make the index match it.
    index.write()?;
    repo.git_repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        COMMIT_MESSAGE,
        &repo.git_repo.find_tree(tree_id)?,
        &[&parent],
    )?;
    Ok(true)
}

#[derive(Clone)]
struct FileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

fn snapshot_targets(root: &Path) -> Result<Vec<FileSnapshot>> {
    target_paths()
        .into_iter()
        .map(|relative| {
            let path = root.join(relative);
            let contents = if path.exists() {
                Some(
                    fs::read(&path)
                        .with_context(|| format!("failed to read {}", path.display()))?,
                )
            } else {
                None
            };
            Ok(FileSnapshot { path, contents })
        })
        .collect()
}

fn restore_targets(snapshots: &[FileSnapshot]) -> Result<()> {
    for snapshot in snapshots {
        match &snapshot.contents {
            Some(contents) => {
                if let Some(parent) = snapshot.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&snapshot.path, contents)?;
            }
            None if snapshot.path.exists() => fs::remove_file(&snapshot.path)?,
            None => {}
        }
    }
    Ok(())
}

/// Install and commit guidance atomically enough for automatic setup: every
/// target file and the real index are restored if writing or committing fails.
pub fn setup_and_commit(repo: &crate::git::Repo, root: &Path) -> Result<bool> {
    preflight_dedicated_commit(repo, root)?;
    let snapshots = snapshot_targets(root)?;
    let result = (|| {
        setup(root, false)?;
        commit_guidance(repo, root)
    })();
    if let Err(error) = result {
        restore_targets(&snapshots)
            .context("failed to restore agent guidance after setup failure")?;
        return Err(error);
    }
    result
}

/// Best-effort only: this deliberately uses the cached public account key and
/// repository announcement, so it neither prompts nor asks a NIP-46 signer to
/// sign. Errors are swallowed by callers because a warning must never affect
/// the operation that discovered it.
pub async fn warn_if_maintainer(
    repo: &crate::git::Repo,
    repo_ref: &crate::repo_ref::RepoRef,
) -> Result<()> {
    use crate::{git::RepoActions, login::get_likely_logged_in_user};
    let root = repo.get_path()?;
    let Ok(Some(account)) = get_likely_logged_in_user(root).await else {
        return Ok(());
    };
    if !repo_ref.maintainers.contains(&account) {
        return Ok(());
    }
    let now = now_secs();
    let warning_seed = format!("{}:{}", repo_ref.selected_maintainer, repo_ref.identifier);
    if !should_sample_warning(&warning_seed, now) {
        return Ok(());
    }
    let status = status(root)?;
    let Some(message) = warning_message(&status) else {
        return Ok(());
    };
    let throttle_key = format!(
        "nostr.agent-guidance-warning-{}",
        status.bundled_version.replace('.', "-")
    );
    let last_seen = repo
        .get_git_config_item(&throttle_key, Some(false))?
        .and_then(|value| value.parse().ok());
    if !should_warn(last_seen, now) {
        return Ok(());
    }
    eprintln!(
        "{}",
        Style::new()
            .fg(Color::Color256(214))
            .apply_to(message)
            .for_stderr()
    );
    // Git config is repository-local machine state, never a tracked-file mutation.
    repo.save_git_config_item(&throttle_key, &now.to_string(), false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ngit-agent-guidance-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn newer_versions_and_throttle_are_decided_without_io() {
        assert!(version_is_newer("1.0", "1.1"));
        assert!(!version_is_newer("1.1", "1.0"));
        assert!(!should_warn(Some(100), 100 + WARNING_INTERVAL_SECS - 1));
        assert!(should_warn(Some(100), 100 + WARNING_INTERVAL_SECS));
    }
    #[test]
    fn warning_sampling_selects_one_day_in_five() {
        let sampled_days = (0..i64::from(WARNING_SAMPLE_DAYS))
            .filter(|day| should_sample_warning("repo-coordinate", day * WARNING_INTERVAL_SECS))
            .count();
        assert_eq!(sampled_days, 1);
        assert_eq!(
            should_sample_warning("repo-coordinate", 2 * WARNING_INTERVAL_SECS),
            should_sample_warning(
                "repo-coordinate",
                (2 + i64::from(WARNING_SAMPLE_DAYS)) * WARNING_INTERVAL_SECS
            )
        );
    }
    #[test]
    fn section_replacement_preserves_surrounding_content() {
        let input = "before\n<!-- ngit-agent-guidance:start -->\nold\n<!-- ngit-agent-guidance:end -->\nafter\n";
        let output = replace_section(Some(input), START, END, "new").unwrap();
        assert!(output.contains("before"));
        assert!(output.contains("new"));
        assert!(output.contains("after"));
        assert!(!output.contains("old"));
    }
    #[test]
    fn warning_messages_are_exact() {
        let status = GuidanceStatus {
            installed: true,
            installed_version: Some("1.0".into()),
            bundled_version: "1.1".into(),
            update_available: true,
            modified_files: vec![],
            managed_files: vec![],
        };
        assert_eq!(
            warning_message(&status).as_deref(),
            Some(
                "warning: newer ngit agent guidance is available (1.0 -> 1.1); run `ngit agent update`"
            )
        );
    }
    #[test]
    fn setup_adopts_instruction_files_and_preserves_policy_neighbors() {
        let root = temp_root();
        fs::write(
            root.join(AGENTS_PATH),
            "# Local instructions\n\nKeep this text.\n",
        )
        .unwrap();
        fs::write(root.join(CLAUDE_PATH), "# Claude instructions\n").unwrap();
        setup(&root, false).unwrap();
        let first = fs::read_to_string(root.join(AGENTS_PATH)).unwrap();
        setup(&root, false).unwrap();
        assert_eq!(first, fs::read_to_string(root.join(AGENTS_PATH)).unwrap());
        assert!(first.contains("Keep this text."));
        assert_eq!(
            fs::read_to_string(root.join(SKILL_PATH)).unwrap(),
            fs::read_to_string(root.join(CLAUDE_SKILL_PATH)).unwrap()
        );
        assert!(
            fs::read_to_string(root.join(CLAUDE_PATH))
                .unwrap()
                .contains("@AGENTS.md")
        );
        assert!(status(&root).unwrap().modified_files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_or_duplicate_markers_are_conflicts() {
        let root = temp_root();
        fs::write(
            root.join(AGENTS_PATH),
            format!("{START}\nfirst\n{END}\n{START}\nsecond\n{END}\n"),
        )
        .unwrap();
        assert!(
            setup(&root, false)
                .unwrap_err()
                .to_string()
                .contains("markers in file are malformed")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_refuses_unmanaged_skill_files_without_force() {
        let root = temp_root();
        let skill = root.join(SKILL_PATH);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(skill, "project skill\n").unwrap();
        assert!(
            setup(&root, false)
                .unwrap_err()
                .to_string()
                .contains("unmanaged file")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn modified_managed_files_refuse_update_and_diff_is_available() {
        let root = temp_root();
        setup(&root, false).unwrap();
        fs::write(root.join(SKILL_PATH), "locally customized").unwrap();
        assert_eq!(status(&root).unwrap().modified_files, vec![SKILL_PATH]);
        assert!(
            setup(&root, false)
                .unwrap_err()
                .to_string()
                .contains("--diff")
        );
        assert!(
            update(&root, false)
                .unwrap_err()
                .to_string()
                .contains("--diff")
        );
        assert!(proposed_diff(&root).unwrap().contains(SKILL_PATH));
        setup(&root, true).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newer_installed_guidance_refuses_downgrade_without_force() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let mut state = load_state(&root).unwrap().unwrap();
        state.version = "999.0.0".into();
        fs::write(
            root.join(STATE_PATH),
            format!("{}\n", serde_json::to_string_pretty(&state).unwrap()),
        )
        .unwrap();
        assert!(
            update(&root, false)
                .unwrap_err()
                .to_string()
                .contains("refusing to downgrade")
        );
        update(&root, true).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
