//! Repository-managed coding-agent guidance.
//!
//! The installed `SKILL.md` is the source of truth for status and upgrades.
//! Existing `AGENTS.md` and `CLAUDE.md` files may receive a compact
//! install-time pointer, but instruction documents remain user-owned and are
//! never created, tracked, or upgraded.

use std::{
    cmp::Ordering,
    ffi::OsStr,
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use console::{Color, Style};
use serde::Serialize;

pub const SKILL_PATH: &str = ".agents/skills/ngit/SKILL.md";
pub const CLAUDE_SKILL_PATH: &str = ".claude/skills/ngit/SKILL.md";
pub const AGENTS_PATH: &str = "AGENTS.md";
pub const CLAUDE_PATH: &str = "CLAUDE.md";
pub const REMINDERS_CONFIG_KEY: &str = "nostr.skill-reminders";
const SKILL_FILE_NAME: &str = "SKILL.md";
const CANONICAL_SKILL: &str = include_str!("../../skills/ngit/SKILL.md");

/// Bundled reference documents installed alongside each `SKILL.md`. Splitting
/// them out keeps a skill activation small: agents load the slim `SKILL.md`
/// and read the matching reference on demand.
const REFERENCE_FILES: &[(&str, &str)] = &[
    (
        "accounts.md",
        include_str!("../../skills/ngit/reference/accounts.md"),
    ),
    ("ci.md", include_str!("../../skills/ngit/reference/ci.md")),
    (
        "containers.md",
        include_str!("../../skills/ngit/reference/containers.md"),
    ),
    (
        "issues.md",
        include_str!("../../skills/ngit/reference/issues.md"),
    ),
    (
        "nsites.md",
        include_str!("../../skills/ngit/reference/nsites.md"),
    ),
    ("prs.md", include_str!("../../skills/ngit/reference/prs.md")),
    (
        "repositories.md",
        include_str!("../../skills/ngit/reference/repositories.md"),
    ),
    (
        "sync-config.md",
        include_str!("../../skills/ngit/reference/sync-config.md"),
    ),
];

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

#[must_use]
pub fn bundled_references() -> &'static [(&'static str, &'static str)] {
    REFERENCE_FILES
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

fn skill_paths() -> [&'static str; 2] {
    [SKILL_PATH, CLAUDE_SKILL_PATH]
}

/// Managed reference locations: `<skill dir>/reference/<file>` for every
/// bundled reference in both discovery paths. Kept static so per-path checks
/// never allocate.
const REFERENCE_PATHS: [&str; 16] = [
    ".agents/skills/ngit/reference/accounts.md",
    ".agents/skills/ngit/reference/ci.md",
    ".agents/skills/ngit/reference/containers.md",
    ".agents/skills/ngit/reference/issues.md",
    ".agents/skills/ngit/reference/nsites.md",
    ".agents/skills/ngit/reference/prs.md",
    ".agents/skills/ngit/reference/repositories.md",
    ".agents/skills/ngit/reference/sync-config.md",
    ".claude/skills/ngit/reference/accounts.md",
    ".claude/skills/ngit/reference/ci.md",
    ".claude/skills/ngit/reference/containers.md",
    ".claude/skills/ngit/reference/issues.md",
    ".claude/skills/ngit/reference/nsites.md",
    ".claude/skills/ngit/reference/prs.md",
    ".claude/skills/ngit/reference/repositories.md",
    ".claude/skills/ngit/reference/sync-config.md",
];

fn reference_paths() -> [&'static str; 16] {
    REFERENCE_PATHS
}

fn is_reference_path(relative: &str) -> bool {
    REFERENCE_PATHS.contains(&relative)
}

fn is_managed_skill_file(relative: &str) -> bool {
    skill_paths().contains(&relative) || is_reference_path(relative)
}

fn validate_managed_skill_file(root: &Path, relative: &str) -> Result<PathBuf> {
    if skill_paths().contains(&relative) {
        validate_skill_path(root, relative)
    } else {
        validate_reference_path(root, relative)
    }
}

fn allowed_paths() -> [&'static str; 4] {
    [AGENTS_PATH, CLAUDE_PATH, SKILL_PATH, CLAUDE_SKILL_PATH]
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

/// Resolve a managed skill or reference location while allowing it to symlink
/// to a canonical copy kept elsewhere in the repository. That covers one
/// managed location pointing at the other and a repository that keeps a single
/// canonical copy outside both. The returned path is the real in-repository
/// file, so callers update a shared target only once without replacing the
/// symlink.
///
/// `allowed_target_name` is the only file name guidance may be written through
/// this symlink to; without that restriction an unrelated repository file
/// could be overwritten with guidance content by following a symlink pointed
/// at it.
fn resolve_managed_symlink(
    root: &Path,
    relative: &str,
    allowed_target_name: &str,
    kind: &str,
) -> Result<PathBuf> {
    let requested = root.join(relative);
    match fs::symlink_metadata(&requested) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return validate_managed_path(root, relative);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", requested.display()));
        }
        Ok(_) => {}
    }
    if let Ok(path) = validate_managed_path(root, relative) {
        return Ok(path);
    }

    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve repository root {}", root.display()))?;
    let resolved = fs::canonicalize(&requested).with_context(|| {
        format!(
            "failed to resolve symlinked repository {kind} {}",
            requested.display()
        )
    })?;
    let Ok(target) = resolved.strip_prefix(&canonical_root) else {
        bail!(
            "refusing to access repository {kind} `{relative}` because its symlink target is outside the repository"
        );
    };
    if !resolved.is_file() {
        bail!("refusing to access repository {kind} `{relative}` because its target is not a file");
    }
    if target.file_name() != Some(OsStr::new(allowed_target_name)) {
        bail!(
            "refusing to access repository {kind} `{relative}` because its symlink target `{}` is not a {allowed_target_name} file",
            target.display()
        );
    }
    Ok(root.join(target))
}

fn validate_skill_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if !skill_paths().contains(&relative) {
        bail!("unexpected repository skill path `{relative}`");
    }
    resolve_managed_symlink(root, relative, SKILL_FILE_NAME, "skill")
}

fn validate_reference_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if !is_reference_path(relative) {
        bail!("unexpected repository reference path `{relative}`");
    }
    let expected_name = relative
        .rsplit_once('/')
        .map(|(_, name)| name)
        .expect("reference path has a file name");
    resolve_managed_symlink(root, relative, expected_name, "reference")
}

fn existing_skill_paths(root: &Path) -> Result<Vec<&'static str>> {
    let mut paths = vec![];
    for relative in skill_paths() {
        match fs::symlink_metadata(root.join(relative)) {
            Ok(_) => {
                let _ = validate_skill_path(root, relative)?;
                paths.push(relative);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect repository skill `{relative}`"));
            }
        }
    }
    Ok(paths)
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

fn policy(skill_path: &str) -> String {
    format!("- For repository collaboration, use ngit and follow `{skill_path}`.")
}

fn append_policy(existing: &str, skill_path: &str) -> String {
    if existing.to_ascii_lowercase().contains("ngit") {
        return existing.to_string();
    }
    let policy = policy(skill_path);
    match existing {
        "" => format!("{policy}\n"),
        content if content.ends_with('\n') => format!("{content}\n{policy}\n"),
        content => format!("{content}\n\n{policy}\n"),
    }
}

fn expected_files(root: &Path, existing_skills: &[&'static str]) -> Result<Vec<(String, String)>> {
    let first_install = existing_skills.is_empty();
    let targets = if first_install {
        skill_paths().to_vec()
    } else {
        existing_skills.to_vec()
    };
    let mut files = Vec::with_capacity(targets.len() * (1 + REFERENCE_FILES.len()));
    for path in targets {
        files.push((path.to_string(), CANONICAL_SKILL.to_string()));
        let Some(dir) = path.rsplit_once('/').map(|(dir, _)| dir) else {
            continue;
        };
        for (name, content) in REFERENCE_FILES {
            files.push((format!("{dir}/reference/{name}"), content.to_string()));
        }
    }
    if !first_install {
        return Ok(files);
    }
    for (relative, skill_path) in [(AGENTS_PATH, SKILL_PATH), (CLAUDE_PATH, CLAUDE_SKILL_PATH)] {
        let path = validate_managed_path(root, relative)?;
        let Some(existing) = read_optional_text(&path)? else {
            continue;
        };
        let instruction = append_policy(&existing, skill_path);
        if instruction != existing {
            files.push((relative.to_string(), instruction));
        }
    }
    Ok(files)
}

pub fn status(root: &Path) -> Result<GuidanceStatus> {
    let bundled_version = bundled_version()?;
    let logical_paths = existing_skill_paths(root)?;
    if logical_paths.is_empty() {
        return Ok(GuidanceStatus {
            installed: false,
            installed_version: None,
            bundled_version,
            update_available: false,
            modified_files: vec![],
            managed_files: vec![],
        });
    }

    let mut versions = vec![];
    let mut update_available = false;
    let mut modified_files = vec![];
    for logical in &logical_paths {
        let path = validate_skill_path(root, logical)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read managed file {}", path.display()))?;
        let version = skill_version(&content);
        let comparison = version
            .as_deref()
            .and_then(|version| compare_versions(version, &bundled_version));
        update_available |= comparison == Some(Ordering::Less);
        if content != CANONICAL_SKILL && comparison != Some(Ordering::Less) {
            let actual = path
                .strip_prefix(root)
                .context("managed skill path outside worktree")?
                .to_string_lossy()
                .into_owned();
            if !modified_files.contains(&actual) {
                modified_files.push(actual);
            }
        }
        versions.push(version);
        let dir = logical
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .context("managed skill path has a directory")?;
        for (name, bundled) in REFERENCE_FILES {
            let relative = format!("{dir}/reference/{name}");
            let path = validate_reference_path(root, &relative)?;
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read managed file {}", path.display())
                    });
                }
            };
            if content != *bundled {
                let actual = path
                    .strip_prefix(root)
                    .context("managed reference path outside worktree")?
                    .to_string_lossy()
                    .into_owned();
                if !modified_files.contains(&actual) {
                    modified_files.push(actual);
                }
            }
        }
    }
    let installed_version = if versions
        .iter()
        .all(|version| version == versions.first().unwrap_or(&None))
    {
        versions.into_iter().next().flatten()
    } else {
        None
    };
    Ok(GuidanceStatus {
        installed: true,
        update_available,
        installed_version,
        bundled_version,
        modified_files,
        managed_files: logical_paths.into_iter().map(str::to_string).collect(),
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
    let existing_skills = existing_skill_paths(root)?;
    let files = expected_files(root, &existing_skills)?;
    for (relative, _) in &files {
        if is_managed_skill_file(relative) {
            let _ = validate_managed_skill_file(root, relative)?;
        } else {
            let _ = validate_managed_path(root, relative)?;
        }
    }
    let bundled = bundled_version()?;
    // Without --force, refuse to clobber locally modified or newer managed
    // files. The reference checks below live in the same guarded block.
    if !force {
        for logical in &existing_skills {
            let path = validate_skill_path(root, logical)?;
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read managed file {}", path.display()))?;
            let Some(installed) = skill_version(&content) else {
                bail!(
                    "refusing to overwrite unrecognized repository skill `{logical}`; rerun with --force to replace it"
                );
            };
            let comparison = compare_versions(&installed, &bundled);
            // An outdated skill is overwritten even when locally modified,
            // because version state cannot distinguish a local edit from an
            // older release. Its reference files share that versioning, so an
            // upgrade updates them freely instead of refusing on content that
            // is merely from an older version.
            if comparison == Some(Ordering::Less) {
                continue;
            }
            match comparison {
                Some(Ordering::Equal) if content == CANONICAL_SKILL => {}
                Some(Ordering::Equal) => {
                    bail!(
                        "refusing to overwrite locally modified managed file `{logical}`; rerun with --force to replace it"
                    );
                }
                Some(Ordering::Greater) => {
                    bail!(
                        "refusing to downgrade ngit repository skill from {installed} to {bundled}; rerun with --force to override"
                    );
                }
                None => {
                    bail!(
                        "refusing to overwrite repository skill `{logical}` with invalid metadata.version `{installed}`; rerun with --force to replace it"
                    );
                }
                Some(Ordering::Less) => unreachable!("outdated skills are handled above"),
            }
            // The skill is at or ahead of the bundled version, so a reference
            // that differs is a local edit worth protecting.
            let dir = logical
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .context("managed skill path has a directory")?;
            for (name, bundled) in REFERENCE_FILES {
                let relative = format!("{dir}/reference/{name}");
                let path = validate_reference_path(root, &relative)?;
                let Some(content) = read_optional_text(&path)? else {
                    continue;
                };
                if content != *bundled {
                    bail!(
                        "refusing to overwrite locally modified managed file `{relative}`; rerun with --force to replace it"
                    );
                }
            }
        }
    }
    for (relative, content) in &files {
        let managed = is_managed_skill_file(relative);
        if !managed
            || fs::symlink_metadata(root.join(relative))
                .is_err_and(|error| error.kind() == ErrorKind::NotFound)
        {
            create_managed_parent_dirs(root, relative)?;
        }
        let path = if managed {
            validate_managed_skill_file(root, relative)?
        } else {
            validate_managed_path(root, relative)?
        };
        if read_optional_text(&path)?.as_deref() != Some(content) {
            fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    Ok(())
}

fn expected_changes(root: &Path) -> Result<Vec<(String, String)>> {
    let existing_skills = existing_skill_paths(root)?;
    let files = expected_files(root, &existing_skills)?;
    let mut changes = vec![];
    for (relative, expected) in files {
        let path = if is_managed_skill_file(&relative) {
            validate_managed_skill_file(root, &relative)?
        } else {
            validate_managed_path(root, &relative)?
        };
        let actual = read_optional_text(&path)?.unwrap_or_default();
        if actual != expected {
            let actual_relative = path
                .strip_prefix(root)
                .context("managed guidance path outside worktree")?
                .to_string_lossy()
                .into_owned();
            if !changes.iter().any(|(path, _)| path == &actual_relative) {
                changes.push((actual_relative, expected));
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceCommitKind {
    Install,
    Upgrade,
}

impl GuidanceCommitKind {
    fn message(self) -> &'static str {
        match self {
            Self::Install => {
                "chore: install ngit repository skill\n\n\
                 Add repository guidance for supported coding agents."
            }
            Self::Upgrade => {
                "chore: upgrade ngit repository skill\n\n\
                 Update repository guidance for supported coding agents."
            }
        }
    }
}

fn validate_target_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path
        .strip_prefix(root)
        .context("guidance path outside worktree")?;
    let relative = relative
        .to_str()
        .context("managed guidance path is not valid UTF-8")?;
    if allowed_paths().contains(&relative) || is_reference_path(relative) {
        return validate_managed_path(root, relative);
    }
    // A managed skill location may symlink to canonical skill files kept
    // elsewhere in the repository. Guidance updates those resolved files, so a
    // guidance commit has to be able to stage them as well.
    if skill_paths()
        .iter()
        .filter_map(|skill| validate_skill_path(root, skill).ok())
        .any(|resolved| resolved == path)
        || reference_paths()
            .iter()
            .filter_map(|reference| validate_reference_path(root, reference).ok())
            .any(|resolved| resolved == path)
    {
        return Ok(path.to_path_buf());
    }
    bail!("unexpected guidance commit path `{relative}`")
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
    kind: GuidanceCommitKind,
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
    let result = commit_guidance_inner(repo, root, target_paths, kind);
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
    kind: GuidanceCommitKind,
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
        kind.message(),
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
    if !repo_ref.is_authorized_maintainer(&account) {
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

    const SKILL_DIRS: [&str; 2] = [".agents/skills/ngit", ".claude/skills/ngit"];

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
    fn bundled_skill_routes_container_publication() {
        assert!(CANONICAL_SKILL.contains("`reference/containers.md`"));
        let (_, reference) = REFERENCE_FILES
            .iter()
            .find(|(name, _)| *name == "containers.md")
            .expect("container reference should be bundled");
        assert!(reference.contains("ncontainer.io/<npub>/<repository>:<tag>"));
    }
    #[test]
    fn generated_commit_messages_have_distinct_subjects_and_bodies() {
        assert_eq!(
            GuidanceCommitKind::Install.message(),
            "chore: install ngit repository skill\n\n\
             Add repository guidance for supported coding agents."
        );
        assert_eq!(
            GuidanceCommitKind::Upgrade.message(),
            "chore: upgrade ngit repository skill\n\n\
             Update repository guidance for supported coding agents."
        );
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
    fn setup_installs_both_skills_without_creating_instruction_files() {
        let root = temp_root();
        setup(&root, false).unwrap();

        assert!(root.join(SKILL_PATH).is_file());
        assert!(root.join(CLAUDE_SKILL_PATH).is_file());
        assert!(!root.join(AGENTS_PATH).exists());
        assert!(!root.join(CLAUDE_PATH).exists());
        assert_eq!(
            status(&root).unwrap().managed_files,
            vec![SKILL_PATH, CLAUDE_SKILL_PATH]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reference_paths_cover_every_managed_reference() {
        let mut expected = vec![];
        for dir in SKILL_DIRS {
            for (name, _) in REFERENCE_FILES {
                expected.push(format!("{dir}/reference/{name}"));
            }
        }
        expected.sort();
        let mut actual = REFERENCE_PATHS
            .iter()
            .map(|path| path.to_string())
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn setup_installs_bundled_reference_files_beside_each_skill() {
        let root = temp_root();
        setup(&root, false).unwrap();

        for (name, content) in REFERENCE_FILES {
            for dir in SKILL_DIRS {
                let path = root.join(format!("{dir}/reference/{name}"));
                assert!(
                    path.is_file(),
                    "missing managed reference {}",
                    path.display()
                );
                assert_eq!(fs::read_to_string(&path).unwrap(), *content);
            }
        }
        assert!(status(&root).unwrap().modified_files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_adds_pointer_to_existing_claude_without_creating_agents() {
        let root = temp_root();
        fs::write(root.join(CLAUDE_PATH), "# Claude instructions\n").unwrap();
        setup(&root, false).unwrap();

        let claude = fs::read_to_string(root.join(CLAUDE_PATH)).unwrap();
        assert!(claude.contains("# Claude instructions"));
        assert!(claude.contains(CLAUDE_SKILL_PATH));
        assert!(!claude.contains("<!--"));
        assert!(root.join(SKILL_PATH).is_file());
        assert!(root.join(CLAUDE_SKILL_PATH).is_file());
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
                .contains(CLAUDE_SKILL_PATH)
        );
        assert!(root.join(SKILL_PATH).is_file());
        assert!(root.join(CLAUDE_SKILL_PATH).is_file());
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
                .contains("failed to inspect repository skill")
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
    fn modified_reference_files_refuse_update_without_force() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let reference = format!("{}/reference/prs.md", SKILL_DIRS[0]);
        fs::write(root.join(&reference), "locally customized").unwrap();
        assert_eq!(
            status(&root).unwrap().modified_files,
            vec![reference.clone()]
        );
        assert!(
            update(&root, false)
                .unwrap_err()
                .to_string()
                .contains(&format!("locally modified managed file `{reference}`"))
        );
        update(&root, true).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn outdated_skill_updates_edited_references_without_force() {
        let root = temp_root();
        setup(&root, false).unwrap();
        fs::write(
            root.join(SKILL_PATH),
            bundled_skill().replace(
                &format!("version: \"{}\"", bundled_version().unwrap()),
                "version: \"0.1\"",
            ),
        )
        .unwrap();
        let reference = format!("{}/reference/prs.md", SKILL_DIRS[0]);
        fs::write(root.join(&reference), "locally customized").unwrap();

        update(&root, false).unwrap();

        let bundled = REFERENCE_FILES
            .iter()
            .find(|(name, _)| *name == "prs.md")
            .map(|(_, content)| *content)
            .unwrap();
        assert_eq!(fs::read_to_string(root.join(&reference)).unwrap(), bundled);
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
        fs::remove_file(root.join(CLAUDE_SKILL_PATH)).unwrap();
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
        assert!(
            !root.join(CLAUDE_SKILL_PATH).exists(),
            "upgrade restored a manually removed skill copy"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrade_preserves_a_claude_only_install() {
        let root = temp_root();
        setup(&root, false).unwrap();
        fs::remove_file(root.join(SKILL_PATH)).unwrap();
        let older = bundled_skill().replace(
            &format!("version: \"{}\"", bundled_version().unwrap()),
            "version: \"0.1\"",
        );
        fs::write(root.join(CLAUDE_SKILL_PATH), older).unwrap();

        update(&root, false).unwrap();

        assert!(!root.join(SKILL_PATH).exists());
        assert_eq!(
            fs::read_to_string(root.join(CLAUDE_SKILL_PATH)).unwrap(),
            bundled_skill()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_preserves_a_symlinked_claude_skill_directory_and_updates_its_target() {
        let root = temp_root();
        setup(&root, false).unwrap();
        fs::remove_dir_all(root.join(".claude/skills/ngit")).unwrap();
        symlink(
            "../../.agents/skills/ngit",
            root.join(".claude/skills/ngit"),
        )
        .unwrap();
        let older = bundled_skill().replace(
            &format!("version: \"{}\"", bundled_version().unwrap()),
            "version: \"0.1\"",
        );
        fs::write(root.join(SKILL_PATH), older).unwrap();

        assert_eq!(
            paths_for_commit(&root).unwrap(),
            vec![root.join(SKILL_PATH)]
        );
        update(&root, false).unwrap();

        assert!(
            fs::symlink_metadata(root.join(".claude/skills/ngit"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(root.join(SKILL_PATH)).unwrap(),
            bundled_skill()
        );
        assert_eq!(
            fs::read_to_string(root.join(CLAUDE_SKILL_PATH)).unwrap(),
            bundled_skill()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_refuses_a_skill_symlink_outside_the_repository() {
        let root = temp_root();
        let outside = root.with_extension("outside-skill");
        fs::create_dir_all(root.join(".agents/skills/ngit")).unwrap();
        fs::write(&outside, bundled_skill()).unwrap();
        symlink(&outside, root.join(SKILL_PATH)).unwrap();

        assert!(
            update(&root, false)
                .unwrap_err()
                .to_string()
                .contains("outside the repository")
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), bundled_skill());

        fs::remove_dir_all(root).unwrap();
        fs::remove_file(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_follows_skill_symlinks_to_a_canonical_repository_copy() {
        let root = temp_root();
        let canonical = root.join("skills/ngit/SKILL.md");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        let canonical_reference = root.join("skills/ngit/reference");
        fs::create_dir_all(&canonical_reference).unwrap();
        for (name, content) in REFERENCE_FILES {
            fs::write(canonical_reference.join(name), content).unwrap();
        }
        let older = bundled_skill().replace(
            &format!("version: \"{}\"", bundled_version().unwrap()),
            "version: \"0.1\"",
        );
        fs::write(&canonical, older).unwrap();
        for relative in skill_paths() {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink("../../../skills/ngit/SKILL.md", &path).unwrap();
        }
        for dir in SKILL_DIRS {
            let path = root.join(format!("{dir}/reference"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink("../../../skills/ngit/reference", &path).unwrap();
        }

        let before = status(&root).unwrap();
        assert!(before.installed);
        assert!(before.update_available);
        assert_eq!(before.installed_version.as_deref(), Some("0.1"));
        assert_eq!(before.managed_files, vec![SKILL_PATH, CLAUDE_SKILL_PATH]);
        // The shared target is updated once, not once per managed location.
        assert_eq!(paths_for_commit(&root).unwrap(), vec![canonical.clone()]);

        update(&root, false).unwrap();

        assert_eq!(fs::read_to_string(&canonical).unwrap(), bundled_skill());
        for (name, content) in REFERENCE_FILES {
            assert_eq!(
                fs::read_to_string(canonical_reference.join(name)).unwrap(),
                *content,
                "upgrade rewrote the shared reference `skills/ngit/reference/{name}`"
            );
        }
        for relative in skill_paths() {
            assert!(
                fs::symlink_metadata(root.join(relative))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "upgrade replaced the `{relative}` symlink"
            );
        }
        let after = status(&root).unwrap();
        assert!(!after.update_available);
        assert!(after.modified_files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_refuses_a_skill_symlink_to_an_unrelated_repository_file() {
        let root = temp_root();
        let instructions = "# Local instructions\n";
        fs::write(root.join(AGENTS_PATH), instructions).unwrap();
        fs::create_dir_all(root.join(SKILL_PATH).parent().unwrap()).unwrap();
        symlink(format!("../../../{AGENTS_PATH}"), root.join(SKILL_PATH)).unwrap();

        assert!(
            update(&root, true)
                .unwrap_err()
                .to_string()
                .contains("is not a SKILL.md file")
        );
        assert_eq!(
            fs::read_to_string(root.join(AGENTS_PATH)).unwrap(),
            instructions
        );
        fs::remove_dir_all(root).unwrap();
    }
}
