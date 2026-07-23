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

/// Prompting is opt-in. Detection is always safe to run; callers must use this
/// decision separately before offering a setup/update action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionPolicy {
    pub command_allows_prompts: bool,
    pub stdin_is_terminal: bool,
    pub json: bool,
    pub defaults: bool,
    pub remote_helper: bool,
}

impl InteractionPolicy {
    #[must_use]
    pub const fn may_prompt(self) -> bool {
        self.command_allows_prompts
            && self.stdin_is_terminal
            && !self.json
            && !self.defaults
            && !self.remote_helper
    }
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
    match (existing.find(start), existing.find(end)) {
        (None, None) => Ok(format!(
            "{}{}",
            existing.trim_end(),
            if existing.trim().is_empty() {
                section
            } else {
                format!("\n\n{section}")
            }
        )),
        (Some(from), Some(end_from)) if end_from >= from => {
            let after = end_from + end.len();
            Ok(format!(
                "{}{}{}",
                &existing[..from],
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

pub fn setup(root: &Path) -> Result<GuidanceStatus> {
    write_guidance(root, false)?;
    status(root)
}

pub fn update(root: &Path) -> Result<GuidanceStatus> {
    write_guidance(root, true).and_then(|_| status(root))
}

fn write_guidance(root: &Path, refuse_modified: bool) -> Result<()> {
    if refuse_modified {
        let current = status(root)?;
        if let Some(path) = current.modified_files.first() {
            bail!(
                "refusing to overwrite locally modified managed file `{path}`; run `ngit agent update --diff`"
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
    let Some(account) = get_likely_logged_in_user(root).await? else {
        return Ok(());
    };
    if !repo_ref.maintainers.contains(&account) {
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
    if !should_warn(last_seen, now_secs()) {
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
    repo.save_git_config_item(&throttle_key, &now_secs().to_string(), false)?;
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
    fn non_interactive_modes_never_prompt() {
        let interactive = InteractionPolicy {
            command_allows_prompts: true,
            stdin_is_terminal: true,
            json: false,
            defaults: false,
            remote_helper: false,
        };
        assert!(interactive.may_prompt());
        assert!(
            !InteractionPolicy {
                json: true,
                ..interactive
            }
            .may_prompt()
        );
        assert!(
            !InteractionPolicy {
                defaults: true,
                ..interactive
            }
            .may_prompt()
        );
        assert!(
            !InteractionPolicy {
                remote_helper: true,
                ..interactive
            }
            .may_prompt()
        );
        assert!(
            !InteractionPolicy {
                stdin_is_terminal: false,
                ..interactive
            }
            .may_prompt()
        );
    }
    #[test]
    fn setup_is_idempotent_preserves_policy_neighbors_and_syncs_claude_copy() {
        let root = temp_root();
        fs::write(
            root.join(AGENTS_PATH),
            "# Local instructions\n\nKeep this text.\n",
        )
        .unwrap();
        setup(&root).unwrap();
        let first = fs::read_to_string(root.join(AGENTS_PATH)).unwrap();
        setup(&root).unwrap();
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
    fn modified_managed_files_refuse_update_and_diff_is_available() {
        let root = temp_root();
        setup(&root).unwrap();
        fs::write(root.join(SKILL_PATH), "locally customized").unwrap();
        assert_eq!(status(&root).unwrap().modified_files, vec![SKILL_PATH]);
        assert!(update(&root).unwrap_err().to_string().contains("--diff"));
        assert!(proposed_diff(&root).unwrap().contains(SKILL_PATH));
        fs::remove_dir_all(root).unwrap();
    }
}
