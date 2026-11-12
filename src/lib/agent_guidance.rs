//! Repository-managed coding-agent guidance.
//!
//! The recommended guidance is a short instruction in the root `AGENTS.md` or
//! `CLAUDE.md` that sends agents to the installed `ngit --help`. Those
//! documents are user-owned: ngit detects the instruction by two markers and
//! never creates, tracks or upgrades the files, though `ngit skill install`
//! appends the instruction to existing ones that do not mention ngit.
//!
//! The optional repository skill carries the same routing. The installed
//! `SKILL.md` is the source of truth for its status and upgrades.

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
/// Suggested instruction for a root `AGENTS.md` or `CLAUDE.md`. `ngit skill
/// --help` shows it for maintainers to copy and reword.
pub const HELP_INSTRUCTION: &str = include_str!("../bin/ngit/help/agent_instruction.txt");
/// An instruction file that mentions all of these is treated as carrying the
/// instruction, however it is worded.
pub const HELP_INSTRUCTION_MARKERS: [&str; 2] = ["ngit --help", "ngit.dev/install"];
/// Added to the upgrade tip while no instruction file carries the insert, so
/// repositories that installed the skill learn of the recommended setup.
const UPGRADE_INSERT_HINT: &str =
    "; `ngit skill --help` describes the AGENTS.md insert now recommended";
pub const SETUP_TIP: &str = "tip: add an AGENTS.md insert or a skill to tell collaborators' coding agents this repository uses ngit; run `ngit skill --help` for details";
/// Environment variable naming one directory to hold the machine-wide skill,
/// for packagers and systems whose agents read skills from somewhere other
/// than the documented defaults.
pub const SYSTEM_SKILLS_DIR_ENV: &str = "NGIT_SYSTEM_SKILLS_DIR";
const SKILL_FILE_NAME: &str = "SKILL.md";
const CANONICAL_SKILL: &str = include_str!("../../skills/ngit/SKILL.md");

/// Directory beside each `SKILL.md` where earlier versions installed reference
/// documents. The skill now routes to the installed binary's `--help`, so an
/// install or upgrade removes the directory whole.
const LEGACY_REFERENCE_DIR: &str = "reference";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuidanceStatus {
    /// A root instruction file sends agents to installed help.
    pub help_instruction: bool,
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

/// Where `ngit skill install` places the skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallScope {
    /// Inside the current Git repository, shared with everyone who clones it.
    Repository,
    /// Inside the user's home directory, available in all of their
    /// repositories.
    Global,
    /// In the machine-wide directories agents read, available to every user.
    System,
}

impl InstallScope {
    /// Resolve the install base and the managed layout for this scope.
    /// `repo_root` is only required for [`InstallScope::Repository`].
    pub fn base(self, repo_root: Option<&Path>) -> Result<(PathBuf, SkillLayout)> {
        match self {
            InstallScope::Repository => {
                let root =
                    repo_root.context("the repository skill scope needs a Git working tree")?;
                Ok((root.to_path_buf(), SkillLayout::repository()))
            }
            InstallScope::Global => Ok((global_base()?, SkillLayout::home())),
            InstallScope::System => Ok(system_base()),
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

/// The skill locations of one install scope, relative to the scope's install
/// base, so every scope shares the same validation and write path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLayout {
    skill_paths: Vec<String>,
    /// Existing instruction files that a first install may append the help
    /// instruction to. Only the repository scope owns instruction files.
    instruction_files: Vec<&'static str>,
}

impl SkillLayout {
    fn from_skill_dirs<'a>(dirs: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            skill_paths: dirs
                .into_iter()
                .map(|dir| format!("{dir}/{SKILL_FILE_NAME}"))
                .collect(),
            instruction_files: vec![],
        }
    }

    /// Both discovery locations, plus the root instruction files.
    fn repository() -> Self {
        Self {
            skill_paths: vec![SKILL_PATH.into(), CLAUDE_SKILL_PATH.into()],
            instruction_files: vec![AGENTS_PATH, CLAUDE_PATH],
        }
    }

    /// Both discovery locations under the user's home directory.
    fn home() -> Self {
        Self {
            instruction_files: vec![],
            ..Self::repository()
        }
    }

    fn is_skill_path(&self, relative: &str) -> bool {
        self.skill_paths.iter().any(|path| path == relative)
    }

    /// Whether any instruction file this layout may append to already exists
    /// at the install base. Asked before offering the install-time edit.
    pub fn instruction_files_present(&self, base: &Path) -> Result<bool> {
        for relative in &self.instruction_files {
            match fs::symlink_metadata(base.join(relative)) {
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
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

/// The user's home directory, the install base of the global scope.
pub fn global_base() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("failed to resolve the user's home directory for the global skill scope")
}

/// Machine-wide locations that agents document reading skills from. Codex
/// reads its admin scope, `/etc/codex/skills`; Claude Code reads
/// `.claude/skills` inside its managed settings directory, `/etc/claude-code`
/// on Linux. Its documentation gives no skills path for macOS, so only Codex
/// is covered there. On Windows the managed directory is
/// `%ProgramFiles%\ClaudeCode`, and the skills path is inferred from it.
fn system_base_with_override(override_dir: Option<&Path>) -> (PathBuf, SkillLayout) {
    if let Some(dir) = override_dir {
        return (dir.to_path_buf(), SkillLayout::from_skill_dirs(["ngit"]));
    }
    if cfg!(windows) {
        let base = std::env::var_os("ProgramFiles")
            .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from)
            .join("ClaudeCode");
        return (base, SkillLayout::from_skill_dirs([".claude/skills/ngit"]));
    }
    let mut dirs = vec!["codex/skills/ngit"];
    if cfg!(target_os = "linux") {
        dirs.push("claude-code/.claude/skills/ngit");
    }
    (PathBuf::from("/etc"), SkillLayout::from_skill_dirs(dirs))
}

/// The install base and layout of the system scope.
#[must_use]
pub fn system_base() -> (PathBuf, SkillLayout) {
    system_base_with_override(
        std::env::var_os(SYSTEM_SKILLS_DIR_ENV)
            .as_deref()
            .map(Path::new),
    )
}

/// Report every file beneath `path`, without following symlinks.
fn report_tree(root: &Path, path: &Path, report: &mut impl FnMut(&Path)) -> Result<()> {
    let entries =
        fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", path.display()))?;
        let child = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", child.display()))?;
        if file_type.is_dir() {
            report_tree(root, &child, report)?;
        } else {
            report(
                child
                    .strip_prefix(root)
                    .context("managed guidance path outside worktree")?,
            );
        }
    }
    Ok(())
}

/// Remove the `reference/` entry beside an installed skill, whatever it holds.
/// A directory is deleted with its contents; a symlink is unlinked and its
/// target left alone. Nothing is touched when the skill directory is itself
/// reached through a symlink, because that directory belongs to another
/// managed location or to the repository's own layout.
fn remove_legacy_references(
    root: &Path,
    logical_skill: &str,
    report: &mut impl FnMut(&Path),
) -> Result<()> {
    let dir = logical_skill
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .context("managed skill path has a directory")?;
    if validate_managed_path(root, dir).is_err() {
        return Ok(());
    }
    let relative = format!("{dir}/{LEGACY_REFERENCE_DIR}");
    let path = root.join(&relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect `{relative}`"));
        }
    };
    let removed = if metadata.is_dir() {
        report_tree(root, &path, report)?;
        fs::remove_dir_all(&path)
    } else {
        report(Path::new(&relative));
        fs::remove_file(&path)
    };
    removed.with_context(|| format!("failed to remove {}", path.display()))
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

    let canonical_root = fs::canonicalize(root).with_context(|| {
        format!(
            "failed to resolve skill install directory {}",
            root.display()
        )
    })?;
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
                        "refusing to access managed guidance path `{relative}` outside {}",
                        root.display()
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

/// Resolve a managed skill location while allowing it to symlink
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

    let canonical_root = fs::canonicalize(root).with_context(|| {
        format!(
            "failed to resolve skill install directory {}",
            root.display()
        )
    })?;
    let resolved = fs::canonicalize(&requested).with_context(|| {
        format!(
            "failed to resolve symlinked ngit {kind} {}",
            requested.display()
        )
    })?;
    let Ok(target) = resolved.strip_prefix(&canonical_root) else {
        bail!(
            "refusing to access ngit {kind} `{relative}` because its symlink target is outside {}",
            root.display()
        );
    };
    if !resolved.is_file() {
        bail!("refusing to access ngit {kind} `{relative}` because its target is not a file");
    }
    if target.file_name() != Some(OsStr::new(allowed_target_name)) {
        bail!(
            "refusing to access ngit {kind} `{relative}` because its symlink target `{}` is not a {allowed_target_name} file",
            target.display()
        );
    }
    Ok(root.join(target))
}

fn validate_skill_path(root: &Path, layout: &SkillLayout, relative: &str) -> Result<PathBuf> {
    if !layout.is_skill_path(relative) {
        bail!("unexpected ngit skill path `{relative}`");
    }
    resolve_managed_symlink(root, relative, SKILL_FILE_NAME, "skill")
}

fn existing_skill_paths(root: &Path, layout: &SkillLayout) -> Result<Vec<String>> {
    let mut paths = vec![];
    for relative in &layout.skill_paths {
        match fs::symlink_metadata(root.join(relative)) {
            Ok(_) => {
                let _ = validate_skill_path(root, layout, relative)?;
                paths.push(relative.clone());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect ngit skill `{relative}`"));
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

/// Whether a root `AGENTS.md` or `CLAUDE.md` already sends agents to installed
/// help. One file must mention every marker; the wording around them is the
/// maintainers' own. Unreadable and non-UTF-8 files count as not carrying it.
#[must_use]
pub fn has_help_instruction(root: &Path) -> bool {
    [AGENTS_PATH, CLAUDE_PATH].iter().any(|relative| {
        fs::read(root.join(relative)).is_ok_and(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            HELP_INSTRUCTION_MARKERS
                .iter()
                .all(|marker| text.contains(marker))
        })
    })
}

fn append_policy(existing: &str) -> String {
    if existing.to_ascii_lowercase().contains("ngit") {
        return existing.to_string();
    }
    let policy = HELP_INSTRUCTION.trim_end();
    match existing {
        "" => format!("{policy}\n"),
        content if content.ends_with('\n') => format!("{content}\n{policy}\n"),
        content => format!("{content}\n\n{policy}\n"),
    }
}

fn expected_files(
    root: &Path,
    layout: &SkillLayout,
    existing_skills: &[String],
    update_instructions: bool,
) -> Result<Vec<(String, String)>> {
    let first_install = existing_skills.is_empty();
    let targets = if first_install {
        &layout.skill_paths
    } else {
        existing_skills
    };
    let mut files = Vec::with_capacity(targets.len() + 2);
    for path in targets {
        files.push((path.clone(), CANONICAL_SKILL.to_string()));
    }
    if !first_install || !update_instructions {
        return Ok(files);
    }
    for relative in layout.instruction_files.iter().copied() {
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
    status_at(root, &SkillLayout::repository(), has_help_instruction(root))
}

/// Status of the skill installed in one scope. Only the repository scope has
/// instruction files, so `help_instruction` is false for the others.
pub fn scope_status(scope: InstallScope, repo_root: Option<&Path>) -> Result<GuidanceStatus> {
    let (base, layout) = scope.base(repo_root)?;
    let help_instruction = scope == InstallScope::Repository && has_help_instruction(&base);
    status_at(&base, &layout, help_instruction)
}

fn status_at(root: &Path, layout: &SkillLayout, help_instruction: bool) -> Result<GuidanceStatus> {
    let bundled_version = bundled_version()?;
    let logical_paths = existing_skill_paths(root, layout)?;
    if logical_paths.is_empty() {
        return Ok(GuidanceStatus {
            help_instruction,
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
        let path = validate_skill_path(root, layout, logical)?;
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
        help_instruction,
        installed: true,
        update_available,
        installed_version,
        bundled_version,
        modified_files,
        managed_files: logical_paths,
    })
}

pub fn setup(root: &Path, force: bool) -> Result<GuidanceStatus> {
    write_guidance(root, &SkillLayout::repository(), force, true, &mut |_| {})?;
    status(root)
}

pub fn update(root: &Path, force: bool) -> Result<GuidanceStatus> {
    update_with_report(root, force, &mut |_| {})
}

/// Update managed guidance and report each repository-relative path immediately
/// before a write or removal is attempted. Reporting before the operation lets
/// callers inspect the worktree even when an I/O error leaves a partial write.
pub fn update_with_report(
    root: &Path,
    force: bool,
    report: &mut impl FnMut(&Path),
) -> Result<GuidanceStatus> {
    write_guidance(root, &SkillLayout::repository(), force, true, report)?;
    status(root)
}

/// Install or update the skill in one scope. `update_instructions` lets a
/// first repository install append the help instruction to existing
/// instruction files; other scopes own none.
pub fn update_scope(
    scope: InstallScope,
    repo_root: Option<&Path>,
    force: bool,
    update_instructions: bool,
    report: &mut impl FnMut(&Path),
) -> Result<GuidanceStatus> {
    let (base, layout) = scope.base(repo_root)?;
    if !base.exists() {
        fs::create_dir_all(&base).with_context(|| {
            format!(
                "failed to create the {} skill directory {}",
                scope.label(),
                base.display()
            )
        })?;
    }
    write_guidance(&base, &layout, force, update_instructions, report)?;
    scope_status(scope, repo_root)
}

fn write_guidance(
    root: &Path,
    layout: &SkillLayout,
    force: bool,
    update_instructions: bool,
    report: &mut impl FnMut(&Path),
) -> Result<()> {
    let existing_skills = existing_skill_paths(root, layout)?;
    let files = expected_files(root, layout, &existing_skills, update_instructions)?;
    for (relative, _) in &files {
        if layout.is_skill_path(relative) {
            let _ = validate_skill_path(root, layout, relative)?;
        } else {
            let _ = validate_managed_path(root, relative)?;
        }
    }
    let bundled = bundled_version()?;
    // Without --force, refuse to clobber a locally modified or newer skill.
    if !force {
        for logical in &existing_skills {
            let path = validate_skill_path(root, layout, logical)?;
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read managed file {}", path.display()))?;
            let Some(installed) = skill_version(&content) else {
                bail!(
                    "refusing to overwrite unrecognized ngit skill `{}`; rerun with --force to replace it",
                    root.join(logical).display()
                );
            };
            let comparison = compare_versions(&installed, &bundled);
            // An outdated skill is overwritten even when locally modified,
            // because version state cannot distinguish a local edit from an
            // older release.
            if comparison == Some(Ordering::Less) {
                continue;
            }
            match comparison {
                Some(Ordering::Equal) if content == CANONICAL_SKILL => {}
                Some(Ordering::Equal) => {
                    bail!(
                        "refusing to overwrite locally modified ngit skill `{}`; rerun with --force to replace it",
                        root.join(logical).display()
                    );
                }
                Some(Ordering::Greater) => {
                    bail!(
                        "refusing to downgrade ngit skill `{}` from {installed} to {bundled}; rerun with --force to override",
                        root.join(logical).display()
                    );
                }
                None => {
                    bail!(
                        "refusing to overwrite ngit skill `{}` with invalid metadata.version `{installed}`; rerun with --force to replace it",
                        root.join(logical).display()
                    );
                }
                Some(Ordering::Less) => unreachable!("outdated skills are handled above"),
            }
        }
    }
    for (relative, content) in &files {
        let managed = layout.is_skill_path(relative);
        if !managed
            || fs::symlink_metadata(root.join(relative))
                .is_err_and(|error| error.kind() == ErrorKind::NotFound)
        {
            create_managed_parent_dirs(root, relative)?;
        }
        let path = if managed {
            validate_skill_path(root, layout, relative)?
        } else {
            validate_managed_path(root, relative)?
        };
        if read_optional_text(&path)?.as_deref() != Some(content) {
            report(
                path.strip_prefix(root)
                    .context("managed guidance path outside worktree")?,
            );
            fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    for logical in &existing_skills {
        remove_legacy_references(root, logical, report)?;
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
    if status.installed {
        status.update_available.then_some(WarningKind::Update)
    } else if status.help_instruction {
        None
    } else {
        Some(WarningKind::Setup)
    }
}

#[must_use]
pub fn warning_message(status: &GuidanceStatus) -> Option<String> {
    match warning_for(status)? {
        WarningKind::Setup => Some(SETUP_TIP.into()),
        WarningKind::Update => Some(format!(
            "tip: run `ngit skill upgrade` (repository skill {} -> {}){}",
            status.installed_version.as_deref().unwrap_or("unknown"),
            status.bundled_version,
            if status.help_instruction {
                ""
            } else {
                UPGRADE_INSERT_HINT
            }
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
    crate::report_message!(
        "info",
        "skill_notice",
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
    /// The skill is a bootstrap. Workflow guidance lives in command help
    /// (`src/bin/ngit/help/`), which changes with the binary.
    #[test]
    fn bundled_skill_routes_to_installed_help() {
        assert!(CANONICAL_SKILL.contains("`ngit --help`"));
        assert!(CANONICAL_SKILL.contains("`ngit pr --help` before that\npush"));
        assert!(
            !CANONICAL_SKILL.contains("reference/"),
            "the skill must not route to reference files that are no longer installed"
        );
        assert!(
            CANONICAL_SKILL.lines().count() <= 60,
            "keep the skill a short bootstrap; put workflow guidance in command help"
        );
    }

    #[test]
    fn warning_messages_are_exact() {
        let status = GuidanceStatus {
            help_instruction: false,
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
                "tip: run `ngit skill upgrade` (repository skill 1.0 -> 1.1); `ngit skill --help` describes the AGENTS.md insert now recommended"
            )
        );
        let status = GuidanceStatus {
            help_instruction: true,
            ..status
        };
        assert_eq!(
            warning_message(&status).as_deref(),
            Some("tip: run `ngit skill upgrade` (repository skill 1.0 -> 1.1)")
        );
    }

    /// Maintainers word their own instruction files, so detection rests on
    /// the two markers alone, and both must be in the same file.
    #[test]
    fn help_instruction_is_detected_by_its_markers() {
        let root = temp_root();
        assert!(!has_help_instruction(&root));
        fs::write(root.join(AGENTS_PATH), "Use ngit; see `ngit --help`.\n").unwrap();
        fs::write(root.join(CLAUDE_PATH), "Install: ngit.dev/install\n").unwrap();
        assert!(!has_help_instruction(&root));

        for relative in [AGENTS_PATH, CLAUDE_PATH] {
            let custom = "# House rules\n\nWe use ngit. Run `ngit --help` first; get it at\nhttps://ngit.dev/install if absent.\n";
            fs::write(root.join(relative), custom).unwrap();
            let status = status(&root).unwrap();
            assert!(status.help_instruction);
            assert!(!status.installed);
            assert_eq!(warning_for(&status), None);
            assert_eq!(fs::read_to_string(root.join(relative)).unwrap(), custom);
            fs::write(root.join(relative), "unrelated\n").unwrap();
        }
        fs::write(root.join(AGENTS_PATH), HELP_INSTRUCTION).unwrap();
        assert!(has_help_instruction(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_reminder_stops_once_an_instruction_or_skill_exists() {
        let root = temp_root();
        let status_now = |root: &Path| status(root).unwrap();
        assert_eq!(warning_for(&status_now(&root)), Some(WarningKind::Setup));
        let tip = warning_message(&status_now(&root)).unwrap();
        assert!(tip.contains("`ngit skill --help`"));

        fs::write(root.join(AGENTS_PATH), b"\xff not utf-8").unwrap();
        assert_eq!(warning_for(&status_now(&root)), Some(WarningKind::Setup));
        fs::remove_file(root.join(AGENTS_PATH)).unwrap();

        setup(&root, false).unwrap();
        let installed = status_now(&root);
        assert!(installed.installed && !installed.help_instruction);
        assert_eq!(warning_for(&installed), None);
        fs::remove_dir_all(root).unwrap();
    }
    fn install_at(base: &Path, layout: &SkillLayout, update_instructions: bool) -> Vec<PathBuf> {
        let mut paths = vec![];
        write_guidance(base, layout, false, update_instructions, &mut |path| {
            paths.push(path.to_path_buf());
        })
        .unwrap();
        paths
    }

    #[test]
    fn system_scope_uses_documented_agent_directories_or_the_override() {
        let overridden = PathBuf::from("/opt/skills");
        let (base, layout) = system_base_with_override(Some(&overridden));
        assert_eq!(base, overridden);
        assert_eq!(layout.skill_paths, ["ngit/SKILL.md"]);
        assert!(layout.instruction_files.is_empty());

        let (base, layout) = system_base_with_override(None);
        if cfg!(target_os = "linux") {
            assert_eq!(base, PathBuf::from("/etc"));
            assert_eq!(
                layout.skill_paths,
                [
                    "codex/skills/ngit/SKILL.md",
                    "claude-code/.claude/skills/ngit/SKILL.md"
                ]
            );
        }
        assert!(layout.instruction_files.is_empty());
    }

    #[test]
    fn home_scope_installs_both_skills_and_edits_no_instruction_file() {
        let home = temp_root();
        let instructions = "# My own agent notes\n";
        fs::write(home.join(AGENTS_PATH), instructions).unwrap();
        let layout = SkillLayout::home();

        let written = install_at(&home, &layout, true);
        assert_eq!(
            written,
            [PathBuf::from(SKILL_PATH), PathBuf::from(CLAUDE_SKILL_PATH)]
        );
        assert_eq!(
            fs::read_to_string(home.join(AGENTS_PATH)).unwrap(),
            instructions
        );
        let status = status_at(&home, &layout, false).unwrap();
        assert!(status.installed && !status.update_available);
        assert!(install_at(&home, &layout, true).is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn single_directory_scope_installs_upgrades_and_drops_legacy_references() {
        let base = temp_root();
        let (_, layout) = system_base_with_override(Some(&base));
        assert_eq!(
            install_at(&base, &layout, false),
            [PathBuf::from("ngit/SKILL.md")]
        );
        assert!(!base.join(".agents").exists());

        fs::write(
            base.join("ngit/SKILL.md"),
            bundled_skill().replace(
                &format!("version: \"{}\"", bundled_version().unwrap()),
                "version: \"0.1\"",
            ),
        )
        .unwrap();
        fs::create_dir_all(base.join("ngit/reference")).unwrap();
        fs::write(base.join("ngit/reference/prs.md"), "old\n").unwrap();
        assert!(status_at(&base, &layout, false).unwrap().update_available);

        let written = install_at(&base, &layout, false);
        assert!(written.contains(&PathBuf::from("ngit/SKILL.md")));
        assert!(written.contains(&PathBuf::from("ngit/reference/prs.md")));
        assert!(!base.join("ngit/reference").exists());
        assert_eq!(
            fs::read_to_string(base.join("ngit/SKILL.md")).unwrap(),
            bundled_skill()
        );
        fs::remove_dir_all(base).unwrap();
    }

    /// Refusals name the file by its full path and do not call a skill
    /// outside a repository a repository skill.
    #[test]
    fn refusals_outside_a_repository_name_the_file_not_a_repository() {
        let home = temp_root();
        let layout = SkillLayout::home();
        install_at(&home, &layout, false);
        let skill = home.join(SKILL_PATH);

        for (content, expected) in [
            ("no front matter\n".to_string(), "unrecognized ngit skill"),
            (
                format!("{}\nlocal edit\n", bundled_skill()),
                "locally modified ngit skill",
            ),
            (
                bundled_skill().replace(
                    &format!("version: \"{}\"", bundled_version().unwrap()),
                    "version: \"999.0\"",
                ),
                "refusing to downgrade ngit skill",
            ),
        ] {
            fs::write(&skill, content).unwrap();
            let error = write_guidance(&home, &layout, false, false, &mut |_| {})
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(error.contains(&skill.display().to_string()), "{error}");
            assert!(!error.contains("repository"), "{error}");
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn first_repository_install_can_leave_instruction_files_alone() {
        let root = temp_root();
        let instructions = "# Local instructions\n";
        fs::write(root.join(AGENTS_PATH), instructions).unwrap();
        let layout = SkillLayout::repository();
        assert!(layout.instruction_files_present(&root).unwrap());

        install_at(&root, &layout, false);
        assert!(root.join(SKILL_PATH).is_file());
        assert_eq!(
            fs::read_to_string(root.join(AGENTS_PATH)).unwrap(),
            instructions
        );
        fs::remove_dir_all(&root).unwrap();
        assert!(
            !SkillLayout::home()
                .instruction_files_present(Path::new("/"))
                .unwrap()
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
    fn upgrade_removes_the_whole_legacy_reference_directory() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let mut legacy = vec![];
        for dir in SKILL_DIRS {
            // Names no ngit version knows about are removed with the rest.
            for name in ["prs.md", "repo-settings.md", "notes/unknown.md"] {
                let path = root.join(format!("{dir}/reference/{name}"));
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, "superseded reference\n").unwrap();
                legacy.push(path);
            }
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
        for path in &legacy {
            let relative = path.strip_prefix(&root).unwrap();
            assert!(paths.iter().any(|path| path == relative), "{paths:?}");
        }
        for dir in SKILL_DIRS {
            assert!(!root.join(format!("{dir}/reference")).exists());
            assert!(root.join(format!("{dir}/SKILL.md")).is_file());
        }
        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert!(paths.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_unlinks_a_reference_symlink_without_touching_its_target() {
        let root = temp_root();
        let canonical_dir = root.join("skills/ngit");
        fs::create_dir_all(canonical_dir.join("reference")).unwrap();
        fs::write(canonical_dir.join("SKILL.md"), bundled_skill()).unwrap();
        let target = canonical_dir.join("reference/prs.md");
        fs::write(&target, "repository-owned file\n").unwrap();
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
        for dir in SKILL_DIRS {
            let link = format!("{dir}/reference");
            assert!(paths.contains(&PathBuf::from(&link)), "{paths:?}");
            assert!(fs::symlink_metadata(root.join(&link)).is_err());
        }
        assert!(target.is_file(), "upgrade followed a reference symlink");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_skill_still_drops_a_lingering_reference_directory() {
        let root = temp_root();
        setup(&root, false).unwrap();
        let path = root.join(format!("{}/reference/repo-settings.md", SKILL_DIRS[0]));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "superseded reference\n").unwrap();

        let (result, paths) = update_report(&root, false);
        result.unwrap();
        assert_eq!(paths, vec![path.strip_prefix(&root).unwrap().to_path_buf()]);

        assert!(!path.exists());
        assert!(status(&root).unwrap().modified_files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_appends_the_instruction_to_existing_claude_without_creating_agents() {
        let root = temp_root();
        fs::write(root.join(CLAUDE_PATH), "# Claude instructions\n").unwrap();
        setup(&root, false).unwrap();

        let claude = fs::read_to_string(root.join(CLAUDE_PATH)).unwrap();
        assert!(claude.contains("# Claude instructions"));
        assert!(claude.ends_with(HELP_INSTRUCTION));
        assert!(!claude.contains("<!--"));
        assert!(root.join(SKILL_PATH).is_file());
        assert!(root.join(CLAUDE_SKILL_PATH).is_file());
        assert!(!root.join(AGENTS_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_appends_the_instruction_to_each_existing_instruction_file() {
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
        for relative in [AGENTS_PATH, CLAUDE_PATH] {
            assert!(
                fs::read_to_string(root.join(relative))
                    .unwrap()
                    .ends_with(&format!("\n\n{HELP_INSTRUCTION}"))
            );
        }
        assert!(status(&root).unwrap().help_instruction);
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
                .contains("unrecognized ngit skill")
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
                .contains("failed to inspect ngit skill")
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
                .contains("symlink target is outside")
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
        let older = bundled_skill().replace(
            &format!("version: \"{}\"", bundled_version().unwrap()),
            "version: \"0.1\"",
        );
        fs::write(&canonical, older).unwrap();
        for relative in [SKILL_PATH, CLAUDE_SKILL_PATH] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink("../../../skills/ngit/SKILL.md", &path).unwrap();
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
        for relative in [SKILL_PATH, CLAUDE_SKILL_PATH] {
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
