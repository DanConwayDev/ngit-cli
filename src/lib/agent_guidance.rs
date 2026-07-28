//! Repository-managed coding-agent guidance.
//!
//! The installed `SKILL.md` is the source of truth for status and upgrades.
//! Existing `AGENTS.md` and `CLAUDE.md` files may receive a compact
//! install-time pointer, but instruction documents remain user-owned and are
//! never created, tracked, or upgraded.

use std::{
    cmp::Ordering,
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use console::{Color, Style};
use serde::Serialize;

pub const SKILL_PATH: &str = ".agents/skills/ngit/SKILL.md";
pub const AGENTS_PATH: &str = "AGENTS.md";
pub const CLAUDE_PATH: &str = "CLAUDE.md";
pub const REMINDERS_CONFIG_KEY: &str = "nostr.skill-reminders";
const CANONICAL_SKILL: &str = include_str!("../../skills/ngit/SKILL.md");

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
    skill_version(CANONICAL_SKILL).context("bundled ngit skill is missing metadata.version")
}

fn skill_version(content: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("version:").map(str::trim) {
            return Some(value.trim_matches('"').to_string()).filter(|value| !value.is_empty());
        }
    }
    None
}

fn allowed_paths() -> [&'static str; 3] {
    [AGENTS_PATH, CLAUDE_PATH, SKILL_PATH]
}

fn validate_managed_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid managed guidance path `{relative}`");
    }

    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve repository root {}", root.display()))?;
    let mut current = root.to_path_buf();
    let components = relative_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "refusing to access managed guidance path `{relative}` because {} is a symlink",
                        current.display()
                    );
                }
                if index + 1 < components.len() && !metadata.is_dir() {
                    bail!(
                        "refusing to access managed guidance path `{relative}` because {} is not a directory",
                        current.display()
                    );
                }
                let resolved = fs::canonicalize(&current).with_context(|| {
                    format!(
                        "failed to resolve managed guidance path {}",
                        current.display()
                    )
                })?;
                if !resolved.starts_with(&canonical_root) {
                    bail!(
                        "refusing to access managed guidance path `{relative}` outside the repository"
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect managed guidance path {}",
                        current.display()
                    )
                });
            }
        }
    }
    Ok(root.join(relative_path))
}

fn create_managed_parent_dirs(root: &Path, relative: &str) -> Result<()> {
    let relative_path = Path::new(relative);
    let Some(parent) = relative_path.parent() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "refusing to create managed guidance path `{relative}` through {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "failed to create managed guidance directory {}",
                                current.display()
                            )
                        });
                    }
                }
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!(
                        "failed to inspect managed guidance directory {}",
                        current.display()
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "refusing to create managed guidance path `{relative}` through {}",
                        current.display()
                    );
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect managed guidance directory {}",
                        current.display()
                    )
                });
            }
        }
    }
    let _ = validate_managed_path(root, relative)?;
    Ok(())
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read managed file {}", path.display()))
        }
    }
}

fn policy() -> String {
    format!("- For repository collaboration, use ngit and follow `{SKILL_PATH}`.")
}

fn append_policy(existing: &str) -> String {
    if existing.to_ascii_lowercase().contains("ngit") {
        return existing.to_string();
    }
    let policy = policy();
    match existing {
        "" => format!("{policy}\n"),
        content if content.ends_with('\n') => format!("{content}\n{policy}\n"),
        content => format!("{content}\n\n{policy}\n"),
    }
}

fn expected_files(root: &Path, first_install: bool) -> Result<Vec<(String, String)>> {
    let mut files = vec![(SKILL_PATH.to_string(), CANONICAL_SKILL.to_string())];
    if !first_install {
        return Ok(files);
    }
    for relative in [AGENTS_PATH, CLAUDE_PATH] {
        let path = validate_managed_path(root, relative)?;
        let Some(existing) = read_optional_text(&path)? else {
            continue;
        };
        let instruction = append_policy(&existing);
        if instruction != existing {
            files.push((relative.to_string(), instruction));
        }
    }
    Ok(files)
}

pub fn status(root: &Path) -> Result<GuidanceStatus> {
    let bundled_version = bundled_version()?;
    let path = validate_managed_path(root, SKILL_PATH)?;
    let Some(content) = read_optional_text(&path)? else {
        return Ok(GuidanceStatus {
            installed: false,
            installed_version: None,
            bundled_version,
            update_available: false,
            modified_files: vec![],
            managed_files: vec![],
        });
    };
    let installed_version = skill_version(&content);
    let update_available = installed_version
        .as_deref()
        .and_then(|version| compare_versions(version, &bundled_version))
        == Some(Ordering::Less);
    let modified_files = if content == CANONICAL_SKILL || update_available {
        vec![]
    } else {
        vec![SKILL_PATH.to_string()]
    };
    Ok(GuidanceStatus {
        installed: true,
        update_available,
        installed_version,
        bundled_version,
        modified_files,
        managed_files: vec![SKILL_PATH.to_string()],
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
    let skill_path = validate_managed_path(root, SKILL_PATH)?;
    let existing = read_optional_text(&skill_path)?;
    let files = expected_files(root, existing.is_none())?;
    for relative in files.iter().map(|(path, _)| path.as_str()) {
        let _ = validate_managed_path(root, relative)?;
    }
    if let Some(content) = &existing {
        let bundled = bundled_version()?;
        if !force {
            let Some(installed) = skill_version(content) else {
                bail!(
                    "refusing to overwrite unrecognized repository skill `{SKILL_PATH}`; rerun with --force to replace it"
                );
            };
            match compare_versions(&installed, &bundled) {
                Some(Ordering::Less) => {}
                Some(Ordering::Equal) if content == CANONICAL_SKILL => {}
                Some(Ordering::Equal) => {
                    bail!(
                        "refusing to overwrite locally modified managed file `{SKILL_PATH}`; rerun with --force to replace it"
                    );
                }
                Some(Ordering::Greater) => {
                    bail!(
                        "refusing to downgrade ngit repository skill from {installed} to {bundled}; rerun with --force to override"
                    );
                }
                None => {
                    bail!(
                        "refusing to overwrite repository skill `{SKILL_PATH}` with invalid metadata.version `{installed}`; rerun with --force to replace it"
                    );
                }
            }
        }
    }
    for (relative, content) in &files {
        create_managed_parent_dirs(root, relative)?;
        let path = validate_managed_path(root, relative)?;
        if read_optional_text(&path)?.as_deref() != Some(content) {
            let _ = validate_managed_path(root, relative)?;
            fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    Ok(())
}

fn expected_changes(root: &Path) -> Result<Vec<(String, String)>> {
    let skill_path = validate_managed_path(root, SKILL_PATH)?;
    let files = expected_files(root, read_optional_text(&skill_path)?.is_none())?;
    let mut changes = vec![];
    for (relative, expected) in files {
        let path = validate_managed_path(root, &relative)?;
        let actual = read_optional_text(&path)?.unwrap_or_default();
        if actual != expected {
            changes.push((relative, expected));
        }
    }
    Ok(changes)
}

#[must_use]
pub fn version_is_newer(installed: &str, bundled: &str) -> bool {
    compare_versions(installed, bundled) == Some(Ordering::Less)
}

fn compare_versions(installed: &str, bundled: &str) -> Option<Ordering> {
    fn parts(value: &str) -> Option<Vec<u64>> {
        value
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()
    }
    match (parts(installed), parts(bundled)) {
        (Some(mut old), Some(mut new)) => {
            old.resize(3, 0);
            new.resize(3, 0);
            Some(old.cmp(&new))
        }
        _ => None,
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
            "tip: install ngit's repository skill so coding agents use ngit instead of GitHub; run `ngit skill install` (or `ngit skill opt-out --local` to stop reminders here)".into(),
        ),
        WarningKind::Update => Some(format!(
            "tip: newer ngit repository skill guidance is available ({} -> {}); run `ngit skill upgrade` (or `ngit skill opt-out --local` to stop reminders here)",
            status.installed_version.as_deref().unwrap_or("unknown"),
            status.bundled_version
        )),
    }
}

pub fn reminders_enabled(repo: &crate::git::Repo) -> Result<bool> {
    use crate::git::RepoActions;

    let Some(value) = repo.get_git_config_item(REMINDERS_CONFIG_KEY, None)? else {
        return Ok(true);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("invalid boolean value `{value}` for git config `{REMINDERS_CONFIG_KEY}`"),
    }
}

pub fn set_reminders_enabled(repo: &crate::git::Repo, enabled: bool) -> Result<()> {
    use crate::git::RepoActions;

    repo.save_git_config_item(
        REMINDERS_CONFIG_KEY,
        if enabled { "true" } else { "false" },
        false,
    )
}

pub fn set_global_reminders_enabled(enabled: bool) -> Result<()> {
    crate::git::save_git_config_item(
        &None,
        REMINDERS_CONFIG_KEY,
        if enabled { "true" } else { "false" },
    )
}

pub fn paths_for_commit(root: &Path) -> Result<Vec<PathBuf>> {
    let paths = expected_changes(root)?
        .into_iter()
        .map(|(path, _)| validate_managed_path(root, &path))
        .collect::<Result<Vec<_>>>()?;
    Ok(paths)
}

const COMMIT_MESSAGE: &str = "chore: update ngit repository skill";

fn validate_target_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path
        .strip_prefix(root)
        .context("guidance path outside worktree")?;
    let relative = relative
        .to_str()
        .context("managed guidance path is not valid UTF-8")?;
    if !allowed_paths().contains(&relative) {
        bail!("unexpected guidance commit path `{relative}`");
    }
    validate_managed_path(root, relative)
}

/// Validate that a guidance-only commit can be made without absorbing any
/// unrelated repository changes. This deliberately happens before setup writes
/// its files, so callers can safely treat a failure as a no-op.
pub fn preflight_dedicated_commit(
    repo: &crate::git::Repo,
    root: &Path,
    target_paths: &[PathBuf],
) -> Result<()> {
    for path in target_paths {
        let _ = validate_target_path(root, path)?;
    }
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
    for path in target_paths {
        let relative = path
            .strip_prefix(root)
            .context("guidance path outside worktree")?;
        let status = match repo.git_repo.status_file(relative) {
            Ok(status) => status,
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                // A target that is absent from HEAD, the index, and the
                // worktree is the normal first-install case.
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect guidance target `{}`", relative.display())
                });
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
                "cannot create a guidance commit while target file `{}` has unstaged changes",
                relative.display()
            );
        }
    }
    Ok(())
}

/// Create a commit containing only the installed guidance files. The caller
/// must run [`preflight_dedicated_commit`] before changing the worktree.
pub fn commit_guidance(
    repo: &crate::git::Repo,
    root: &Path,
    target_paths: &[PathBuf],
) -> Result<bool> {
    let index_path = repo.git_repo.path().join("index");
    let original_index = if index_path.exists() {
        Some(
            fs::read(&index_path)
                .with_context(|| format!("failed to read {}", index_path.display()))?,
        )
    } else {
        None
    };
    let result = commit_guidance_inner(repo, root, target_paths);
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

fn commit_guidance_inner(
    repo: &crate::git::Repo,
    root: &Path,
    target_paths: &[PathBuf],
) -> Result<bool> {
    let head = repo
        .git_repo
        .head()
        .context("cannot create a dedicated guidance commit without HEAD")?;
    let parent = head.peel_to_commit()?;
    let tree = parent.tree()?;
    let mut index = repo.git_repo.index()?;
    index.read_tree(&tree)?;
    for path in target_paths {
        let path = validate_target_path(root, path)?;
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
    if !reminders_enabled(repo)? {
        return Ok(());
    }
    let Ok(Some(account)) = get_likely_logged_in_user(root).await else {
        return Ok(());
    };
    if !repo_ref.maintainers.contains(&account) {
        return Ok(());
    }
    let status = status(root)?;
    let Some(message) = warning_message(&status) else {
        return Ok(());
    };
    eprintln!(
        "{}",
        Style::new()
            .fg(Color::Color256(214))
            .apply_to(message)
            .for_stderr()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
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
    fn newer_versions_are_decided_without_io() {
        assert!(version_is_newer("1.0", "1.1"));
        assert!(!version_is_newer("1.1", "1.0"));
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
                "tip: newer ngit repository skill guidance is available (1.0 -> 1.1); run `ngit skill upgrade` (or `ngit skill opt-out --local` to stop reminders here)"
            )
        );
    }
    #[test]
    fn setup_does_not_create_instruction_files() {
        let root = temp_root();
        setup(&root, false).unwrap();

        assert!(root.join(SKILL_PATH).is_file());
        assert!(!root.join(AGENTS_PATH).exists());
        assert!(!root.join(CLAUDE_PATH).exists());
        assert_eq!(status(&root).unwrap().managed_files, vec![SKILL_PATH]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_adds_pointer_to_existing_claude_without_creating_agents() {
        let root = temp_root();
        fs::write(root.join(CLAUDE_PATH), "# Claude instructions\n").unwrap();
        setup(&root, false).unwrap();

        let claude = fs::read_to_string(root.join(CLAUDE_PATH)).unwrap();
        assert!(claude.contains("# Claude instructions"));
        assert!(claude.contains(SKILL_PATH));
        assert!(!claude.contains("<!--"));
        assert!(root.join(SKILL_PATH).is_file());
        assert!(!root.join(AGENTS_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_adds_pointer_to_each_existing_instruction_file() {
        let root = temp_root();
        fs::write(
            root.join(AGENTS_PATH),
            "# Local instructions\n\nKeep this text.\n",
        )
        .unwrap();
        let claude = "# Claude instructions\n";
        fs::write(root.join(CLAUDE_PATH), claude).unwrap();
        setup(&root, false).unwrap();

        assert!(
            fs::read_to_string(root.join(AGENTS_PATH))
                .unwrap()
                .contains("Keep this text.")
        );
        assert!(
            fs::read_to_string(root.join(AGENTS_PATH))
                .unwrap()
                .contains(SKILL_PATH)
        );
        assert!(
            fs::read_to_string(root.join(CLAUDE_PATH))
                .unwrap()
                .contains(SKILL_PATH)
        );
        assert!(root.join(SKILL_PATH).is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_preserves_instruction_wording_that_already_mentions_ngit() {
        let root = temp_root();
        let custom = "# Local policy\n\nUse NGIT for collaboration.\n";
        fs::write(root.join(AGENTS_PATH), custom).unwrap();

        setup(&root, false).unwrap();

        assert_eq!(fs::read_to_string(root.join(AGENTS_PATH)).unwrap(), custom);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_refuses_unrecognized_skill_files_without_force() {
        let root = temp_root();
        let skill = root.join(SKILL_PATH);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(skill, "project skill\n").unwrap();
        assert!(
            setup(&root, false)
                .unwrap_err()
                .to_string()
                .contains("unrecognized repository skill")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_refuses_to_replace_non_utf8_instruction_files() {
        let root = temp_root();
        let original = [0xff, 0xfe, 0xfd];
        fs::write(root.join(AGENTS_PATH), original).unwrap();

        for force in [false, true] {
            assert!(
                setup(&root, force)
                    .unwrap_err()
                    .to_string()
                    .contains("failed to read managed file")
            );
            assert_eq!(fs::read(root.join(AGENTS_PATH)).unwrap(), original);
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_ignores_non_utf8_instruction_files() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let custom = [0xff, 0xfe, 0xfd];
        fs::write(root.join(AGENTS_PATH), custom).unwrap();

        assert!(paths_for_commit(&root).unwrap().is_empty());
        update(&root, false).unwrap();

        assert_eq!(fs::read(root.join(AGENTS_PATH)).unwrap(), custom);
        assert!(status(&root).unwrap().modified_files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_validates_selected_paths_before_writing() {
        let root = temp_root();
        fs::write(root.join(".agents"), "not a directory\n").unwrap();

        assert!(
            setup(&root, false)
                .unwrap_err()
                .to_string()
                .contains("is not a directory")
        );
        assert!(!root.join(SKILL_PATH).exists());
        assert!(!root.join(AGENTS_PATH).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn setup_refuses_symlinked_managed_file_even_with_force() {
        let root = temp_root();
        let outside = root.with_extension("outside");
        fs::write(&outside, "outside content\n").unwrap();
        symlink(&outside, root.join(AGENTS_PATH)).unwrap();

        for force in [false, true] {
            assert!(
                setup(&root, force)
                    .unwrap_err()
                    .to_string()
                    .contains("is a symlink")
            );
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside content\n");
        }

        fs::remove_dir_all(root).unwrap();
        fs::remove_file(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn setup_refuses_symlinked_managed_parent() {
        let root = temp_root();
        let outside = root.with_extension("outside-dir");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join(".agents")).unwrap();

        assert!(
            setup(&root, true)
                .unwrap_err()
                .to_string()
                .contains("is a symlink")
        );
        assert!(fs::read_dir(&outside).unwrap().next().is_none());

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn modified_managed_files_refuse_update_without_force() {
        let root = temp_root();
        setup(&root, false).unwrap();
        fs::write(root.join(SKILL_PATH), "locally customized").unwrap();
        assert_eq!(status(&root).unwrap().modified_files, vec![SKILL_PATH]);
        assert!(
            setup(&root, false)
                .unwrap_err()
                .to_string()
                .contains("rerun with --force")
        );
        assert!(
            update(&root, false)
                .unwrap_err()
                .to_string()
                .contains("rerun with --force")
        );
        setup(&root, true).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newer_installed_guidance_refuses_downgrade_without_force() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let newer = bundled_skill().replace(
            &format!("version: \"{}\"", bundled_version().unwrap()),
            "version: \"999.0.0\"",
        );
        fs::write(root.join(SKILL_PATH), newer).unwrap();
        assert!(
            update(&root, false)
                .unwrap_err()
                .to_string()
                .contains("refusing to downgrade")
        );
        update(&root, true).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn older_installed_guidance_is_upgraded_from_its_version() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let older = bundled_skill().replace(
            &format!("version: \"{}\"", bundled_version().unwrap()),
            "version: \"0.1\"",
        );
        fs::write(root.join(SKILL_PATH), older).unwrap();

        let before = status(&root).unwrap();
        assert!(before.update_available);
        assert!(before.modified_files.is_empty());
        update(&root, false).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(SKILL_PATH)).unwrap(),
            bundled_skill()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
