//! Diagnose the installation selected by this process's Git and PATH.

use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, time::timeout};

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn lookup(name: &str, paths: &OsStr) -> Option<PathBuf> {
    env::split_paths(paths)
        .map(|dir| dir.join(format!("{name}{}", env::consts::EXE_SUFFIX)))
        .find(|path| executable(path))
}

fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        if a == b {
            return true;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (a.metadata(), b.metadata()) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    false
}

async fn probe(program: &Path, args: &[&str], path: Option<&OsStr>) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let output = timeout(Duration::from_secs(5), command.output())
        .await
        .context("installation lookup timed out")??;
    if !output.status.success() {
        bail!("{} failed its installation check", program.display());
    }
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

pub struct Lookup {
    pub conflict: bool,
    pub warnings: Vec<String>,
    helper: Option<PathBuf>,
    git_path: std::ffi::OsString,
}

impl Lookup {
    /// Compare with the selected release, including explicit older versions.
    pub async fn check_version(&mut self, expected: &str) {
        if let Some(helper) = &self.helper {
            match probe(helper, &["--version"], Some(&self.git_path)).await {
                Ok(version) if version.trim() == format!("v{expected}") => {},
                Ok(version) => self.warnings.push(format!(
                    "Git's helper {} reports {version}; expected v{expected}. Rerun the one-line installer for a standalone repair. For Cargo, reinstall with cargo install ngit --locked --force using the original --root and desired --version; otherwise use the owning package manager.", helper.display())),
                Err(error) => self.warnings.push(format!("Cannot verify Git's helper {}: {error}", helper.display())),
            }
        }
    }
}

/// Git prepends its exec-path (including GIT_EXEC_PATH) to its inherited PATH.
/// This cannot predict a later invocation through a different shell or wrapper.
pub async fn inspect(current: &Path) -> Result<Lookup> {
    let path = env::var_os("PATH").context("PATH is unset; cannot verify Git's helper")?;
    inspect_with_path(current, &path).await
}

async fn inspect_with_path(current: &Path, path: &OsStr) -> Result<Lookup> {
    let current = current
        .canonicalize()
        .with_context(|| format!("Cannot resolve ngit at {}", current.display()))?;
    if env::split_paths(path).any(|entry| !entry.is_absolute()) {
        bail!(
            "PATH contains relative or empty entries; use absolute PATH entries to verify Git's helper (Git may change directories)"
        );
    }
    let git = lookup("git", path).context("Git is not on PATH; cannot verify its helper")?;
    let exec_path = probe(&git, &["--exec-path"], Some(path)).await?;
    if exec_path.is_empty() {
        bail!("Git returned an empty exec-path");
    }
    let exec_path = env::current_dir()?.join(exec_path);
    let git_path = env::join_paths(std::iter::once(exec_path).chain(env::split_paths(path)))?;
    let helper = lookup("git-remote-nostr", &git_path);
    let expected = current
        .parent()
        .context("ngit has no parent directory")?
        .join(format!("git-remote-nostr{}", env::consts::EXE_SUFFIX));
    let mut result = Lookup {
        conflict: false,
        warnings: Vec::new(),
        helper,
        git_path,
    };
    if let Some(active) = lookup("ngit", path) {
        if !same_file(&active, &current) {
            result.conflict = true;
            result.warnings.push(format!("PATH selects ngit at {}, but this updater belongs to {}. Update the active installation through its owner or explicitly select the intended installation.", active.display(), current.display()));
        }
    } else {
        result.warnings.push(format!(
            "ngit is not on PATH; this installation is {}",
            current.display()
        ));
    }
    match &result.helper {
        Some(active) if !same_file(active, &expected) => {
            result.conflict = true;
            result.warnings.push(format!("Git selects {} instead of {}. Update or uninstall the conflicting copy through its owner (Cargo, Nix, or your package manager). A helper in Git's exec-path takes precedence even after changing PATH. No conflicting files were removed.", active.display(), expected.display()));
        },
        None => result.warnings.push(format!("Git cannot find git-remote-nostr; expected {}. Rerun the one-line installer for a standalone repair. For Cargo, reinstall with cargo install ngit --locked --force using the original --root and desired --version; otherwise use the owning package manager.", expected.display())),
        _ => {},
    }
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use super::*;

    #[test]
    fn exec_directory_precedes_path_and_nonexecutables_are_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let exec = temp.path().join("git exec ");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&exec).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let old = exec.join("git-remote-nostr");
        let new = bin.join("git-remote-nostr");
        fs::write(&old, "old").unwrap();
        fs::write(&new, "new").unwrap();
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755)).unwrap();
        let path = env::join_paths([&exec, &bin]).unwrap();
        assert_eq!(lookup("git-remote-nostr", &path), Some(new));
        fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(lookup("git-remote-nostr", &path), Some(old));
    }

    #[test]
    fn symlinks_and_hardlinks_are_the_same_installation() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let link = temp.path().join("link");
        let hard = temp.path().join("hard");
        fs::write(&original, "binary").unwrap();
        symlink(&original, &link).unwrap();
        fs::hard_link(&original, &hard).unwrap();
        assert!(same_file(&original, &link));
        assert!(same_file(&original, &hard));
        fs::remove_file(&hard).unwrap();
        fs::write(&hard, "binary").unwrap();
        assert!(!same_file(&original, &hard));
    }
    #[tokio::test]
    async fn detects_the_same_shadowed_helper_as_real_git() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let exec = temp.path().join("git exec ");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&exec).unwrap();
        let marker = temp.path().join("selected");
        let helper = exec.join("git-remote-nostr");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf old > '{}'\necho v2.6.1\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(bin.join("ngit"), "binary").unwrap();
        fs::write(bin.join("git-remote-nostr"), "binary").unwrap();
        fs::set_permissions(bin.join("ngit"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            bin.join("git"),
            format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", exec.display()),
        )
        .unwrap();
        fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = env::join_paths([&bin]).unwrap();
        let mut inspected = inspect_with_path(&bin.join("ngit"), &path).await.unwrap();
        assert!(inspected.conflict);
        assert_eq!(inspected.helper, Some(helper));
        inspected.check_version("3.0.3").await;
        assert!(
            inspected
                .warnings
                .iter()
                .any(|warning| warning.contains("v2.6.1"))
        );
        fs::remove_file(&marker).unwrap();
        let output = Command::new("git")
            .arg(format!("--exec-path={}", exec.display()))
            .args(["remote-nostr", "--version"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert!(marker.exists(), "real Git must select the same helper");

        // Once the conflicting copy is removed, an owned but stale helper
        // still needs a diagnostic even if the ngit executable is current.
        fs::remove_file(exec.join("git-remote-nostr")).unwrap();
        let owned_helper = bin.join("git-remote-nostr");
        fs::write(&owned_helper, "#!/bin/sh\necho v2.6.1\n").unwrap();
        fs::set_permissions(&owned_helper, fs::Permissions::from_mode(0o755)).unwrap();
        let mut inspected = inspect_with_path(&bin.join("ngit"), &path).await.unwrap();
        assert!(!inspected.conflict);
        inspected.check_version("3.0.3").await;
        assert!(!inspected.warnings.is_empty());
        fs::remove_file(&owned_helper).unwrap();
        let inspected = inspect_with_path(&bin.join("ngit"), &path).await.unwrap();
        assert!(
            !inspected.conflict,
            "an owned missing helper can be repaired"
        );
        assert!(inspected.helper.is_none());
        assert!(!inspected.warnings.is_empty());
        assert!(
            inspect_with_path(&bin.join("ngit"), OsStr::new("."))
                .await
                .is_err()
        );
    }
}
