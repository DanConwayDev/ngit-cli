//! Managed coding-agent guidance.
//!
//! The installed `SKILL.md` is the source of truth for status and upgrades.
//! Existing `AGENTS.md` and `CLAUDE.md` files may receive a compact
//! install-time pointer, but instruction documents remain user-owned and are
//! never created, tracked, or upgraded. Installs target one of three scopes:
//! the current repository, the user's home directory, or a machine-wide
//! directory.

use std::{
    cmp::Ordering,
    ffi::OsStr,
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use console::{Color, Style};
use directories::BaseDirs;
use serde::Serialize;

pub const SKILL_PATH: &str = ".agents/skills/ngit/SKILL.md";
pub const CLAUDE_SKILL_PATH: &str = ".claude/skills/ngit/SKILL.md";
pub const AGENTS_PATH: &str = "AGENTS.md";
pub const CLAUDE_PATH: &str = "CLAUDE.md";
pub const REMINDERS_CONFIG_KEY: &str = "nostr.skill-reminders";
/// Environment variable overriding the machine-wide skill directory, for
/// packagers and tests that cannot write to `/usr/local/share`.
pub const SYSTEM_SKILLS_DIR_ENV: &str = "NGIT_SYSTEM_SKILLS_DIR";
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

/// Reference files bundled by earlier ngit versions and since merged into
/// other references. An upgrade deletes any installed copy so a stale document
/// cannot outlive the guidance that superseded it.
const RETIRED_REFERENCE_FILES: &[&str] = &["repo-settings.md"];

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

/// Skill discovery directories, relative to the install base, shared by the
/// repository and user-home scopes. Codex-style agents read `.agents/skills`
/// and Claude Code reads `.claude/skills`.
const SHARED_SKILL_DIRS: [&str; 2] = [".agents/skills/ngit", ".claude/skills/ngit"];

/// Single skill directory, relative to the machine-wide install base, used by
/// the system scope. Machine-wide locations avoid dot-prefixed directories.
const CANONICAL_SKILL_DIR: &str = "ngit";

/// Where `ngit skill install` places the managed skill files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallScope {
    /// Inside the current Git repository, shared with everyone who clones it.
    Repository,
    /// Inside the user's home directory, available in all of their repositories.
    Global,
    /// In a machine-wide directory, available to every user.
    System,
}

impl InstallScope {
    /// Resolve the absolute install base and the managed layout for this
    /// scope. `repo_root` is only required for [`InstallScope::Repository`].
    pub fn base(self, repo_root: Option<&Path>) -> Result<(PathBuf, SkillLayout)> {
        match self {
            InstallScope::Repository => {
                let root =
                    repo_root.context("the repository skill scope needs a Git working tree")?;
                Ok((root.to_path_buf(), SkillLayout::repository()))
            }
            InstallScope::Global => Ok((global_base()?, SkillLayout::shared())),
            InstallScope::System => Ok((system_base(), SkillLayout::canonical())),
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            InstallScope::Repository => "repository",
            InstallScope::Global => "global",
            InstallScope::System => "system",
        }
    }
}

/// The managed skill files of one install scope, described relative to the
/// scope's install base so every scope shares the same write and validation
/// engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLayout {
    /// Logical skill directories relative to the install base.
    skill_dirs: Vec<String>,
    /// Existing instruction files that may receive an install-time pointer,
    /// paired with the managed skill path the pointer names. Only the
    /// repository scope owns instruction files.
    instruction_files: Vec<(String, String)>,
}

impl SkillLayout {
    fn from_skill_dirs(skill_dirs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            skill_dirs: skill_dirs.into_iter().map(Into::into).collect(),
            instruction_files: vec![],
        }
    }

    /// Repository layout: both discovery locations, plus pointers from
    /// existing `AGENTS.md` / `CLAUDE.md` files.
    fn repository() -> Self {
        Self {
            instruction_files: vec![
                (AGENTS_PATH.to_string(), SKILL_PATH.to_string()),
                (CLAUDE_PATH.to_string(), CLAUDE_SKILL_PATH.to_string()),
            ],
            ..Self::from_skill_dirs(SHARED_SKILL_DIRS)
        }
    }

    /// User-home layout: both discovery locations, no instruction files.
    fn shared() -> Self {
        Self::from_skill_dirs(SHARED_SKILL_DIRS)
    }

    /// Machine-wide layout: one canonical `ngit` directory.
    fn canonical() -> Self {
        Self::from_skill_dirs([CANONICAL_SKILL_DIR])
    }

    fn skill_paths(&self) -> Vec<PathBuf> {
        self.skill_dirs
            .iter()
            .map(|dir| Path::new(dir).join(SKILL_FILE_NAME))
            .collect()
    }

    fn reference_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.skill_dirs.len() * REFERENCE_FILES.len());
        for dir in &self.skill_dirs {
            for (name, _) in REFERENCE_FILES {
                paths.push(Path::new(dir).join("reference").join(name));
            }
        }
        paths
    }

    fn retired_reference_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.skill_dirs.len() * RETIRED_REFERENCE_FILES.len());
        for dir in &self.skill_dirs {
            for name in RETIRED_REFERENCE_FILES {
                paths.push(Path::new(dir).join("reference").join(name));
            }
        }
        paths
    }

    fn is_skill_path(&self, relative: &Path) -> bool {
        self.skill_paths().iter().any(|path| path == relative)
    }

    fn is_reference_path(&self, relative: &Path) -> bool {
        self.reference_paths().iter().any(|path| path == relative)
    }

    fn is_retired_reference_path(&self, relative: &Path) -> bool {
        self.retired_reference_paths()
            .iter()
            .any(|path| path == relative)
    }

    fn is_managed(&self, relative: &Path) -> bool {
        self.is_skill_path(relative) || self.is_reference_path(relative)
    }

    /// Whether any instruction file this layout may point from already exists
    /// at the install base. Asked before offering the install-time pointer.
    pub fn instruction_files_present(&self, base: &Path) -> Result<bool> {
        for (relative, _) in &self.instruction_files {
            match fs::symlink_metadata(base.join(relative)) {
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect instruction file `{relative}`")
                    });
                }
            }
        }
        Ok(false)
    }
}

/// Resolve the user's home directory, the install base of the global scope.
pub fn global_base() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("failed to resolve the user's home directory for the global skill scope")
}

/// Resolve the machine-wide install base, honoring an explicit override.
fn system_base_with_override(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if cfg!(windows) {
        std::env::var_os("ALLUSERSPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("agents")
            .join("skills")
    } else {
        PathBuf::from("/usr/local/share/agents/skills")
    }
}

/// Resolve the machine-wide install base for the system scope.
pub fn system_base() -> PathBuf {
    system_base_with_override(
        std::env::var_os(SYSTEM_SKILLS_DIR_ENV)
            .as_deref()
            .map(Path::new),
    )
}

fn validate_retired_reference_path(
    base: &Path,
    layout: &SkillLayout,
    relative: &Path,
) -> Result<PathBuf> {
    if !layout.is_retired_reference_path(relative) {
        bail!("unexpected retired reference path `{}`", relative.display());
    }
    let expected_name = relative
        .file_name()
        .and_then(OsStr::to_str)
        .expect("retired reference path has a file name");
    resolve_managed_symlink(base, relative, expected_name, "reference")
}

/// Installed copies of retired references beside each existing skill,
/// resolved through any canonical symlink. Only files that exist are returned.
fn retired_files(
    base: &Path,
    layout: &SkillLayout,
    existing_skills: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    for logical in existing_skills {
        let Some(dir) = logical.parent() else {
            continue;
        };
        for name in RETIRED_REFERENCE_FILES {
            let relative = dir.join("reference").join(name);
            match fs::symlink_metadata(base.join(&relative)) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect retired reference `{}`",
                            relative.display()
                        )
                    });
                }
            }
            let path = validate_retired_reference_path(base, layout, &relative)?;
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn validate_managed_path(base: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid managed guidance path `{}`", relative.display());
    }

    let canonical_base = fs::canonicalize(base)
        .with_context(|| format!("failed to resolve install base {}", base.display()))?;
    let mut current = base.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "refusing to access managed guidance path `{}` because {} is a symlink",
                        relative.display(),
                        current.display()
                    );
                }
                if index + 1 < components.len() && !metadata.is_dir() {
                    bail!(
                        "refusing to access managed guidance path `{}` because {} is not a directory",
                        relative.display(),
                        current.display()
                    );
                }
                let resolved = fs::canonicalize(&current).with_context(|| {
                    format!(
                        "failed to resolve managed guidance path {}",
                        current.display()
                    )
                })?;
                if !resolved.starts_with(&canonical_base) {
                    bail!(
                        "refusing to access managed guidance path `{}` outside the install base",
                        relative.display()
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
    Ok(base.join(relative))
}

/// Resolve a managed skill or reference location while allowing it to symlink
/// to a canonical copy kept elsewhere under the install base. That covers one
/// managed location pointing at the other and a repository that keeps a single
/// canonical copy outside both. The returned path is the real managed file,
/// so callers update a shared target only once without replacing the symlink.
///
/// `allowed_target_name` is the only file name guidance may be written through
/// this symlink to; without that restriction an unrelated file under the
/// install base could be overwritten with guidance content by following a
/// symlink pointed at it.
fn resolve_managed_symlink(
    base: &Path,
    relative: &Path,
    allowed_target_name: &str,
    kind: &str,
) -> Result<PathBuf> {
    let requested = base.join(relative);
    match fs::symlink_metadata(&requested) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return validate_managed_path(base, relative);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", requested.display()));
        }
        Ok(_) => {}
    }
    if let Ok(path) = validate_managed_path(base, relative) {
        return Ok(path);
    }

    let canonical_base = fs::canonicalize(base)
        .with_context(|| format!("failed to resolve install base {}", base.display()))?;
    let resolved = fs::canonicalize(&requested).with_context(|| {
        format!(
            "failed to resolve symlinked managed {kind} {}",
            requested.display()
        )
    })?;
    let Ok(target) = resolved.strip_prefix(&canonical_base) else {
        bail!(
            "refusing to access managed {kind} `{}` because its symlink target is outside the install base",
            relative.display()
        );
    };
    if !resolved.is_file() {
        bail!(
            "refusing to access managed {kind} `{}` because its target is not a file",
            relative.display()
        );
    }
    if target.file_name() != Some(OsStr::new(allowed_target_name)) {
        bail!(
            "refusing to access managed {kind} `{}` because its symlink target `{}` is not a {allowed_target_name} file",
            relative.display(),
            target.display()
        );
    }
    Ok(base.join(target))
}

fn validate_skill_path(base: &Path, layout: &SkillLayout, relative: &Path) -> Result<PathBuf> {
    if !layout.is_skill_path(relative) {
        bail!("unexpected managed skill path `{}`", relative.display());
    }
    resolve_managed_symlink(base, relative, SKILL_FILE_NAME, "skill")
}

fn validate_reference_path(base: &Path, layout: &SkillLayout, relative: &Path) -> Result<PathBuf> {
    if !layout.is_reference_path(relative) {
        bail!("unexpected managed reference path `{}`", relative.display());
    }
    let expected_name = relative
        .file_name()
        .and_then(OsStr::to_str)
        .expect("reference path has a file name");
    resolve_managed_symlink(base, relative, expected_name, "reference")
}

fn validate_managed_skill_file(
    base: &Path,
    layout: &SkillLayout,
    relative: &Path,
) -> Result<PathBuf> {
    if layout.is_skill_path(relative) {
        validate_skill_path(base, layout, relative)
    } else {
        validate_reference_path(base, layout, relative)
    }
}

fn existing_skill_paths(base: &Path, layout: &SkillLayout) -> Result<Vec<PathBuf>> {
    let mut paths = vec![];
    for relative in layout.skill_paths() {
        match fs::symlink_metadata(base.join(&relative)) {
            Ok(_) => {
                let _ = validate_skill_path(base, layout, &relative)?;
                paths.push(relative);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect managed skill `{}`", relative.display())
                });
            }
        }
    }
    Ok(paths)
}

fn create_managed_parent_dirs(base: &Path, relative: &Path) -> Result<()> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = base.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "refusing to create managed guidance path `{}` through {}",
                        relative.display(),
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
                        "refusing to create managed guidance path `{}` through {}",
                        relative.display(),
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
    let _ = validate_managed_path(base, relative)?;
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

fn expected_files(
    base: &Path,
    layout: &SkillLayout,
    existing_skills: &[PathBuf],
    update_instructions: bool,
) -> Result<Vec<(String, String)>> {
    let first_install = existing_skills.is_empty();
    let targets = if first_install {
        layout.skill_paths()
    } else {
        existing_skills.to_vec()
    };
    let mut files = Vec::with_capacity(targets.len() * (1 + REFERENCE_FILES.len()));
    for path in targets {
        files.push((
            path.to_string_lossy().into_owned(),
            CANONICAL_SKILL.to_string(),
        ));
        let Some(dir) = path.parent() else {
            continue;
        };
        for (name, content) in REFERENCE_FILES {
            files.push((
                dir.join("reference")
                    .join(name)
                    .to_string_lossy()
                    .into_owned(),
                content.to_string(),
            ));
        }
    }
    if !first_install || !update_instructions {
        return Ok(files);
    }
    for (relative, skill_path) in &layout.instruction_files {
        let path = validate_managed_path(base, Path::new(relative))?;
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

/// Status of the repository-scope guidance, the scope reminder notices and
/// `ngit init` suggestions inspect.
pub fn status(root: &Path) -> Result<GuidanceStatus> {
    status_at(root, &SkillLayout::repository())
}

/// Status of the guidance installed at one scope. `repo_root` is only
/// required for [`InstallScope::Repository`].
pub fn scope_status(scope: InstallScope, repo_root: Option<&Path>) -> Result<GuidanceStatus> {
    let (base, layout) = scope.base(repo_root)?;
    status_at(&base, &layout)
}

fn status_at(base: &Path, layout: &SkillLayout) -> Result<GuidanceStatus> {
    let bundled_version = bundled_version()?;
    let logical_paths = existing_skill_paths(base, layout)?;
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
        let path = validate_skill_path(base, layout, logical)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read managed file {}", path.display()))?;
        let version = skill_version(&content);
        let comparison = version
            .as_deref()
            .and_then(|version| compare_versions(version, &bundled_version));
        update_available |= comparison == Some(Ordering::Less);
        if content != CANONICAL_SKILL && comparison != Some(Ordering::Less) {
            let actual = path
                .strip_prefix(base)
                .context("managed skill path outside the install base")?
                .to_string_lossy()
                .into_owned();
            if !modified_files.contains(&actual) {
                modified_files.push(actual);
            }
        }
        versions.push(version);
        let dir = logical
            .parent()
            .context("managed skill path has a directory")?;
        for (name, bundled) in REFERENCE_FILES {
            let relative = dir.join("reference").join(name);
            let path = validate_reference_path(base, layout, &relative)?;
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
                    .strip_prefix(base)
                    .context("managed reference path outside the install base")?
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
        managed_files: logical_paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    })
}

pub fn setup(root: &Path, force: bool) -> Result<GuidanceStatus> {
    update_scope(
        InstallScope::Repository,
        Some(root),
        force,
        true,
        &mut |_| {},
    )
}

pub fn update(root: &Path, force: bool) -> Result<GuidanceStatus> {
    update_scope(
        InstallScope::Repository,
        Some(root),
        force,
        true,
        &mut |_| {},
    )
}

/// Update managed guidance and report each repository-relative path immediately
/// before a write or removal is attempted. Reporting before the operation lets
/// callers inspect the worktree even when an I/O error leaves a partial write.
pub fn update_with_report(
    root: &Path,
    force: bool,
    report: &mut impl FnMut(&Path),
) -> Result<GuidanceStatus> {
    update_scope(InstallScope::Repository, Some(root), force, true, report)
}

/// Update the managed guidance installed at one scope and report each
/// base-relative path immediately before a write or removal is attempted.
/// `update_instructions` gates the one-time pointer added to existing
/// instruction files, which only the repository scope has. `repo_root` is
/// only required for [`InstallScope::Repository`].
pub fn update_scope(
    scope: InstallScope,
    repo_root: Option<&Path>,
    force: bool,
    update_instructions: bool,
    report: &mut impl FnMut(&Path),
) -> Result<GuidanceStatus> {
    let (base, layout) = scope.base(repo_root)?;
    create_install_base(scope, &base)?;
    write_guidance(&base, &layout, force, update_instructions, report)?;
    status_at(&base, &layout)
}

/// Ensure the install base of a scope exists before guidance validation walks
/// into it. Repository roots and home directories already do; the system-wide
/// directory may not exist yet on a fresh machine.
fn create_install_base(scope: InstallScope, base: &Path) -> Result<()> {
    if base.exists() {
        return Ok(());
    }
    let description = match scope {
        InstallScope::Repository => "repository skill",
        InstallScope::Global => "user skill",
        InstallScope::System => {
            return fs::create_dir_all(base).context(format!(
                "failed to create the system-wide skill directory {}; writing there may require elevated permissions",
                base.display()
            ));
        }
    };
    fs::create_dir_all(base).context(format!(
        "failed to create the {description} directory {}",
        base.display()
    ))
}

fn write_guidance(
    base: &Path,
    layout: &SkillLayout,
    force: bool,
    update_instructions: bool,
    report: &mut impl FnMut(&Path),
) -> Result<()> {
    let existing_skills = existing_skill_paths(base, layout)?;
    let files = expected_files(base, layout, &existing_skills, update_instructions)?;
    for (relative, _) in &files {
        let relative_path = Path::new(relative);
        if layout.is_managed(relative_path) {
            let _ = validate_managed_skill_file(base, layout, relative_path)?;
        } else {
            let _ = validate_managed_path(base, relative_path)?;
        }
    }
    let bundled = bundled_version()?;
    // Without --force, refuse to clobber locally modified or newer managed
    // files. The reference checks below live in the same guarded block.
    if !force {
        for logical in &existing_skills {
            let path = validate_skill_path(base, layout, logical)?;
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read managed file {}", path.display()))?;
            let Some(installed) = skill_version(&content) else {
                bail!(
                    "refusing to overwrite unrecognized repository skill `{}`; rerun with --force to replace it",
                    logical.display()
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
                        "refusing to overwrite locally modified managed file `{}`; rerun with --force to replace it",
                        logical.display()
                    );
                }
                Some(Ordering::Greater) => {
                    bail!(
                        "refusing to downgrade ngit repository skill from {installed} to {bundled}; rerun with --force to override"
                    );
                }
                None => {
                    bail!(
                        "refusing to overwrite repository skill `{}` with invalid metadata.version `{installed}`; rerun with --force to replace it",
                        logical.display()
                    );
                }
                Some(Ordering::Less) => unreachable!("outdated skills are handled above"),
            }
            // The skill is at or ahead of the bundled version, so a reference
            // that differs is a local edit worth protecting.
            let dir = logical
                .parent()
                .context("managed skill path has a directory")?;
            for (name, bundled) in REFERENCE_FILES {
                let relative = dir.join("reference").join(name);
                let path = validate_reference_path(base, layout, &relative)?;
                let Some(content) = read_optional_text(&path)? else {
                    continue;
                };
                if content != *bundled {
                    bail!(
                        "refusing to overwrite locally modified managed file `{}`; rerun with --force to replace it",
                        relative.display()
                    );
                }
            }
        }
    }
    for (relative, content) in &files {
        let relative_path = Path::new(relative);
        let managed = layout.is_managed(relative_path);
        if !managed
            || fs::symlink_metadata(base.join(relative_path))
                .is_err_and(|error| error.kind() == ErrorKind::NotFound)
        {
            create_managed_parent_dirs(base, relative_path)?;
        }
        let path = if managed {
            validate_managed_skill_file(base, layout, relative_path)?
        } else {
            validate_managed_path(base, relative_path)?
        };
        if read_optional_text(&path)?.as_deref() != Some(content) {
            report(
                path.strip_prefix(base)
                    .context("managed guidance path outside the install base")?,
            );
            fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    for path in retired_files(base, layout, &existing_skills)? {
        report(
            path.strip_prefix(base)
                .context("managed guidance path outside the install base")?,
        );
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
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
            "tip: run `ngit skill upgrade` (repository skill {} -> {})",
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

    fn update_report(root: &Path, force: bool) -> (Result<GuidanceStatus>, Vec<PathBuf>) {
        let mut paths = vec![];
        let result = update_with_report(root, force, &mut |path| {
            let path = path.to_path_buf();
            if !paths.contains(&path) {
                paths.push(path);
            }
        });
        (result, paths)
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
            Some("tip: run `ngit skill upgrade` (repository skill 1.0 -> 1.1)")
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
        let mut actual = SkillLayout::repository()
            .reference_paths()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            SkillLayout::shared().reference_paths(),
            SkillLayout::repository().reference_paths()
        );
    }

    #[test]
    fn retired_reference_paths_cover_every_retired_reference() {
        let mut expected = vec![];
        for dir in SKILL_DIRS {
            for name in RETIRED_REFERENCE_FILES {
                expected.push(format!("{dir}/reference/{name}"));
            }
        }
        expected.sort();
        let mut actual = SkillLayout::repository()
            .retired_reference_paths()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, expected);
        for name in RETIRED_REFERENCE_FILES {
            assert!(
                REFERENCE_FILES.iter().all(|(bundled, _)| bundled != name),
                "{name} is both bundled and retired"
            );
        }
    }

    #[test]
    fn upgrade_removes_retired_references() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let mut retired = vec![];
        for dir in SKILL_DIRS {
            let path = root.join(format!("{dir}/reference/repo-settings.md"));
            fs::write(&path, "superseded reference\n").unwrap();
            retired.push(path);
        }
        fs::write(
            root.join(SKILL_PATH),
            bundled_skill().replace(
                &format!("version: \"{}\"", bundled_version().unwrap()),
                "version: \"0.1\"",
            ),
        )
        .unwrap();

        let (result, paths) = update_report(&root, false);
        result.unwrap();
        for path in &retired {
            let relative = path.strip_prefix(&root).unwrap();
            assert!(paths.iter().any(|path| path == relative));
        }

        for path in &retired {
            assert!(!path.exists(), "{} survived the upgrade", path.display());
        }
        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert!(paths.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_skill_still_drops_a_lingering_retired_reference() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let path = root.join(format!("{}/reference/repo-settings.md", SKILL_DIRS[0]));
        fs::write(&path, "superseded reference\n").unwrap();

        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert_eq!(paths, vec![path.strip_prefix(&root).unwrap().to_path_buf()]);

        assert!(!path.exists());
        assert!(status(&root).unwrap().modified_files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_removes_a_retired_reference_behind_a_canonical_symlink() {
        let root = temp_root();
        let canonical_dir = root.join("skills/ngit");
        fs::create_dir_all(canonical_dir.join("reference")).unwrap();
        fs::write(
            canonical_dir.join("SKILL.md"),
            bundled_skill().replace(
                &format!("version: \"{}\"", bundled_version().unwrap()),
                "version: \"0.1\"",
            ),
        )
        .unwrap();
        for (name, content) in REFERENCE_FILES {
            fs::write(canonical_dir.join("reference").join(name), content).unwrap();
        }
        let retired = canonical_dir.join("reference/repo-settings.md");
        fs::write(&retired, "superseded reference\n").unwrap();
        for dir in SKILL_DIRS {
            fs::create_dir_all(root.join(dir)).unwrap();
            symlink(
                "../../../skills/ngit/SKILL.md",
                root.join(dir).join("SKILL.md"),
            )
            .unwrap();
            symlink(
                "../../../skills/ngit/reference",
                root.join(dir).join("reference"),
            )
            .unwrap();
        }

        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert!(
            paths.iter().any(|path| root.join(path) == retired),
            "{paths:?}"
        );

        assert!(!retired.exists());
        assert_eq!(
            fs::read_to_string(canonical_dir.join("SKILL.md")).unwrap(),
            bundled_skill()
        );
        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert!(paths.is_empty());
        fs::remove_dir_all(root).unwrap();
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

        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert!(paths.is_empty());

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
                .contains("failed to inspect managed skill")
        );
        assert!(!root.join(SKILL_PATH).exists());
        assert!(!root.join(AGENTS_PATH).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_update_reports_paths_attempted_before_the_error() {
        let root = temp_root();
        let mut paths = vec![];
        let mut blocked_later_path = false;

        let result = update_with_report(&root, false, &mut |path| {
            paths.push(path.to_path_buf());
            if !blocked_later_path {
                fs::create_dir_all(root.join(CLAUDE_SKILL_PATH)).unwrap();
                blocked_later_path = true;
            }
        });

        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("failed to read managed file"),
            "{error:#}"
        );
        assert!(paths.contains(&PathBuf::from(SKILL_PATH)));
        assert!(root.join(SKILL_PATH).is_file());
        assert!(!paths.contains(&PathBuf::from(CLAUDE_SKILL_PATH)));
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

        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert_eq!(paths, vec![PathBuf::from(SKILL_PATH)]);

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
                .contains("outside the install base")
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
        for relative in SkillLayout::repository().skill_paths() {
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
        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert_eq!(paths, vec![PathBuf::from("skills/ngit/SKILL.md")]);

        assert_eq!(fs::read_to_string(&canonical).unwrap(), bundled_skill());
        for (name, content) in REFERENCE_FILES {
            assert_eq!(
                fs::read_to_string(canonical_reference.join(name)).unwrap(),
                *content,
                "upgrade rewrote the shared reference `skills/ngit/reference/{name}`"
            );
        }
        for relative in SkillLayout::repository().skill_paths() {
            assert!(
                fs::symlink_metadata(root.join(&relative))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "upgrade replaced the `{}` symlink",
                relative.display()
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

    #[test]
    fn layouts_target_their_discovery_directories() {
        let repository = SkillLayout::repository();
        assert_eq!(
            repository.skill_paths(),
            vec![PathBuf::from(SKILL_PATH), PathBuf::from(CLAUDE_SKILL_PATH)]
        );

        let with_instructions = temp_root();
        fs::write(with_instructions.join(AGENTS_PATH), "# Local\n").unwrap();
        assert!(
            repository
                .instruction_files_present(&with_instructions)
                .unwrap()
        );
        fs::remove_dir_all(with_instructions).unwrap();
        let without_instructions = temp_root();
        assert!(
            !repository
                .instruction_files_present(&without_instructions)
                .unwrap()
        );
        fs::remove_dir_all(without_instructions).unwrap();

        // The user-home scope shares the repository's discovery directories
        // but owns no instruction files to point from.
        let shared = SkillLayout::shared();
        assert_eq!(shared.skill_paths(), repository.skill_paths());
        assert!(shared.instruction_files.is_empty());

        // The system scope installs one canonical, non-hidden directory.
        let canonical = SkillLayout::canonical();
        assert_eq!(
            canonical.skill_paths(),
            vec![PathBuf::from("ngit/SKILL.md")]
        );
        assert!(canonical.instruction_files.is_empty());
        assert!(
            canonical
                .reference_paths()
                .iter()
                .all(|path| path.starts_with("ngit/reference/"))
        );
    }

    #[test]
    fn system_base_honors_the_override_and_falls_back_to_the_platform_default() {
        let overridden = temp_root();
        assert_eq!(system_base_with_override(Some(&overridden)), overridden);
        #[cfg(unix)]
        assert_eq!(
            system_base_with_override(None),
            PathBuf::from("/usr/local/share/agents/skills")
        );
    }

    #[test]
    fn first_install_can_skip_instruction_file_pointers() {
        let root = temp_root();
        let instructions = "# Local instructions\n";
        fs::write(root.join(AGENTS_PATH), instructions).unwrap();
        let layout = SkillLayout::repository();

        write_guidance(&root, &layout, false, false, &mut |_| {}).unwrap();

        assert!(root.join(SKILL_PATH).is_file());
        assert_eq!(
            fs::read_to_string(root.join(AGENTS_PATH)).unwrap(),
            instructions,
            "a declined instruction update still rewrote AGENTS.md"
        );

        write_guidance(&root, &layout, false, true, &mut |_| {}).unwrap();

        // Once the skill exists the install is no longer a first install, so
        // instruction files stay untouched either way.
        assert_eq!(
            fs::read_to_string(root.join(AGENTS_PATH)).unwrap(),
            instructions
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_scope_install_skips_instruction_files() {
        let root = temp_root();
        fs::write(root.join(AGENTS_PATH), "# Local instructions\n").unwrap();
        let layout = SkillLayout::shared();

        write_guidance(&root, &layout, false, true, &mut |_| {}).unwrap();

        assert!(root.join(SKILL_PATH).is_file());
        assert!(root.join(CLAUDE_SKILL_PATH).is_file());
        assert_eq!(
            fs::read_to_string(root.join(AGENTS_PATH)).unwrap(),
            "# Local instructions\n",
            "the user-home scope must not point from repository instruction files"
        );
        let status = status_at(&root, &layout).unwrap();
        assert!(status.installed);
        assert_eq!(
            status.managed_files,
            vec![SKILL_PATH.to_string(), CLAUDE_SKILL_PATH.to_string()]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_scope_install_uses_one_directory() {
        let root = temp_root();
        let layout = SkillLayout::canonical();

        write_guidance(&root, &layout, false, true, &mut |_| {}).unwrap();

        assert!(root.join("ngit/SKILL.md").is_file());
        for (name, content) in REFERENCE_FILES {
            assert_eq!(
                fs::read_to_string(root.join(format!("ngit/reference/{name}"))).unwrap(),
                *content
            );
        }
        assert!(!root.join(".agents").exists());
        assert!(!root.join(".claude").exists());
        let status = status_at(&root, &layout).unwrap();
        assert!(status.installed);
        assert_eq!(status.managed_files, vec!["ngit/SKILL.md".to_string()]);
        fs::remove_dir_all(root).unwrap();
    }
}
