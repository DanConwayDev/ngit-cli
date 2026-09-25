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
            result.warnings.push(format!("Git selects {} instead of {}. Verified standalone copies can be consolidated; Cargo, Nix and unreceipted copies require their original installation method. A helper in Git's exec-path takes precedence even after changing PATH.", active.display(), expected.display()));
        },
        None => result.warnings.push(format!("Git cannot find git-remote-nostr; expected {}. Rerun the one-line installer for a standalone repair. For Cargo, reinstall with cargo install ngit --locked --force using the original --root and desired --version; otherwise use the owning package manager.", expected.display())),
        _ => {},
    }
    Ok(result)
}

const RECEIPT: &str = ".ngit-install-receipt.json";

fn standalone_version(directory: &Path) -> Result<String> {
    if directory.parent().is_some_and(|root| {
        [".crates.toml", ".crates2.json"]
            .iter()
            .any(|name| root.join(name).exists())
    }) {
        bail!(
            "Cargo metadata exists beside {}; resolve ownership through Cargo before consolidating",
            directory.display()
        );
    }
    if directory.starts_with("/nix/store") {
        bail!("Nix installations cannot be consolidated");
    }
    for name in [
        format!("ngit{}", env::consts::EXE_SUFFIX),
        format!("git-remote-nostr{}", env::consts::EXE_SUFFIX),
        RECEIPT.to_owned(),
    ] {
        let path = directory.join(name);
        if !path
            .symlink_metadata()
            .with_context(|| format!("Cannot inspect standalone file {}", path.display()))?
            .file_type()
            .is_file()
        {
            bail!("{} is not a regular standalone file", path.display());
        }
    }
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join(RECEIPT))?)?;
    if receipt["schema"] != 1 || receipt["method"] != "standalone" {
        bail!("invalid standalone receipt in {}", directory.display());
    }
    let version = receipt["version"]
        .as_str()
        .context("receipt has no version")?;
    semver::Version::parse(version)?;
    Ok(version.to_owned())
}

async fn verify_pair(directory: &Path, version: &str) -> Result<()> {
    for (name, expected) in [
        ("ngit", format!("ngit {version}")),
        ("git-remote-nostr", format!("v{version}")),
    ] {
        let path = directory.join(format!("{name}{}", env::consts::EXE_SUFFIX));
        if probe(&path, &["--version"], None).await?.trim() != expected {
            bail!(
                "{} does not match its standalone receipt; repair it before consolidating",
                path.display()
            );
        }
    }
    Ok(())
}

/// Return an obsolete standalone directory only when both complete pairs are
/// receipted and the active ngit is the one the caller intends to retain.
pub async fn consolidation_candidate(current: &Path, target: &str) -> Result<Option<PathBuf>> {
    let current = current
        .canonicalize()
        .with_context(|| format!("Cannot resolve ngit at {}", current.display()))?;
    let inspected = inspect(&current).await?;
    if !inspected.conflict {
        return Ok(None);
    }
    let path = env::var_os("PATH").context("PATH is unset")?;
    if !lookup("ngit", &path).is_some_and(|active| same_file(&active, &current)) {
        bail!("Select the ngit installation you want to retain on PATH before consolidating");
    }
    let retained = current
        .parent()
        .context("ngit has no installation directory")?;
    let retained_version = standalone_version(retained)?;
    verify_pair(retained, &retained_version).await?;
    let helper = inspected.helper.context("Git helper is missing")?;
    // Do not follow a final executable symlink to delete its target elsewhere.
    if !helper
        .symlink_metadata()
        .with_context(|| format!("Cannot inspect Git helper {}", helper.display()))?
        .file_type()
        .is_file()
    {
        bail!(
            "Resolve the conflicting helper symlink through its installation method (for example Nix): {}",
            helper.display()
        );
    }
    let obsolete = helper
        .parent()
        .context("helper has no directory")?
        .canonicalize()
        .with_context(|| format!("Cannot resolve directory containing {}", helper.display()))?;
    if obsolete == retained {
        bail!("Cannot consolidate within the retained directory");
    }
    let old_version = standalone_version(&obsolete)?;
    if semver::Version::parse(&old_version)? > semver::Version::parse(target)? {
        bail!(
            "Conflicting standalone {} is newer ({old_version}); select that installation or resolve it manually",
            obsolete.display()
        );
    }
    verify_pair(&obsolete, &old_version).await?;
    Ok(Some(obsolete))
}

/// Quarantine only the two binaries and receipt, prove lookup is fixed, then
/// delete the quarantine. Any failure before deletion restores the old pair.
pub async fn consolidate(current: &Path, version: &str) -> Result<()> {
    let Some(obsolete) = consolidation_candidate(current, version).await? else {
        return Ok(());
    };
    let retained = current
        .canonicalize()?
        .parent()
        .context("ngit has no directory")?
        .to_path_buf();
    verify_pair(&retained, version).await?;
    let mut locks = Vec::new();
    for directory in [&retained, &obsolete] {
        let lock = directory.join(".ngit-install-lock");
        if let Err(error) = std::fs::create_dir(&lock) {
            for lock in &locks {
                let _ = std::fs::remove_dir(lock);
            }
            bail!(
                "Cannot lock {} for consolidation: {error}. No conflicting files were removed; check permissions or an active installer",
                directory.display()
            );
        }
        locks.push(lock);
    }
    let mut preserve_lock = false;
    let result = consolidate_locked(current, version, &obsolete, &mut preserve_lock).await;
    for lock in locks {
        if preserve_lock && lock == obsolete.join(".ngit-install-lock") {
            continue;
        }
        let _ = std::fs::remove_dir(lock);
    }
    result
}

async fn consolidate_locked(
    current: &Path,
    version: &str,
    obsolete: &Path,
    preserve_lock: &mut bool,
) -> Result<()> {
    // Revalidate after acquiring both locks.
    if consolidation_candidate(current, version).await?.as_deref() != Some(obsolete) {
        bail!("Installation selection changed; retry consolidation");
    }
    verify_pair(
        current
            .canonicalize()?
            .parent()
            .context("ngit has no directory")?,
        version,
    )
    .await?;
    let backup = tempfile::Builder::new()
        .prefix(".ngit-retired-")
        .tempdir_in(obsolete)?;
    let mut moved = Vec::new();
    let result: Result<()> = async {
        for name in [
            format!("ngit{}", env::consts::EXE_SUFFIX),
            format!("git-remote-nostr{}", env::consts::EXE_SUFFIX),
            RECEIPT.to_owned(),
        ] {
            let from = obsolete.join(&name);
            let to = backup.path().join(name);
            std::fs::rename(&from, &to).with_context(|| {
                format!(
                    "Cannot retire {}; check directory permissions",
                    from.display()
                )
            })?;
            moved.push((from, to));
        }
        let mut selected = inspect(current).await?;
        selected.check_version(version).await;
        if !selected.warnings.is_empty() {
            bail!("Another conflict remains: {}", selected.warnings.join("; "));
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        for (from, to) in moved.iter().rev() {
            if let Err(restore) = std::fs::rename(to, from) {
                *preserve_lock = true;
                let recovery = backup.keep();
                bail!(
                    "{error}; restoration failed: {restore}. Recover remaining files from {}",
                    recovery.display()
                );
            }
        }
        return Err(error.context("Conflicting standalone installation was preserved"));
    }
    backup.close().with_context(|| {
        format!(
            "Consolidation succeeded but retired files in {} need manual cleanup",
            obsolete.display()
        )
    })?;
    eprintln!(
        "Removed obsolete standalone ngit files from {}; Git now selects the retained pair",
        obsolete.display()
    );
    Ok(())
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
    #[tokio::test]
    async fn consolidation_subprocess() {
        let Some(current) = env::var_os("NGIT_TEST_CONSOLIDATE") else {
            return;
        };
        let result = consolidate(Path::new(&current), "3.0.3").await;
        if env::var_os("NGIT_TEST_EXPECT_FAILURE").is_some() {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
    }

    #[test]
    fn consolidation_removes_only_verified_files_and_rolls_back_conflicts() {
        for scenario in [
            "success",
            "invalid receipt",
            "another helper",
            "locked",
            "broken retained",
            "Cargo metadata",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let retained = temp.path().join("retained");
            let obsolete = temp.path().join("obsolete");
            let third = temp.path().join("third");
            for dir in [&retained, &obsolete, &third] {
                fs::create_dir(dir).unwrap();
            }
            for (dir, version) in [(&retained, "3.0.3"), (&obsolete, "2.6.1")] {
                fs::write(
                    dir.join(RECEIPT),
                    format!(r#"{{"schema":1,"method":"standalone","version":"{version}"}}"#),
                )
                .unwrap();
                for (name, prefix) in [("ngit", "ngit "), ("git-remote-nostr", "v")] {
                    let path = dir.join(name);
                    fs::write(&path, format!("#!/bin/sh\necho '{prefix}{version}'\n")).unwrap();
                    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            fs::write(obsolete.join("unrelated"), "keep").unwrap();
            match scenario {
                "Cargo metadata" => fs::write(temp.path().join(".crates2.json"), "{}").unwrap(),
                "invalid receipt" => fs::write(obsolete.join(RECEIPT), "invalid").unwrap(),
                "another helper" => {
                    fs::copy(
                        obsolete.join("git-remote-nostr"),
                        third.join("git-remote-nostr"),
                    )
                    .unwrap();
                }
                "locked" => fs::create_dir(obsolete.join(".ngit-install-lock")).unwrap(),
                "broken retained" => fs::write(
                    retained.join("git-remote-nostr"),
                    "#!/bin/sh\necho v1.0.0\n",
                )
                .unwrap(),
                _ => {}
            }
            let path = env::join_paths(
                [third, retained.clone()]
                    .into_iter()
                    .chain(env::split_paths(&env::var_os("PATH").unwrap())),
            )
            .unwrap();
            let mut child = std::process::Command::new(env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "install_lookup::tests::consolidation_subprocess",
                    "--nocapture",
                ])
                .env("NGIT_TEST_CONSOLIDATE", retained.join("ngit"))
                .env("PATH", path)
                .env("GIT_EXEC_PATH", &obsolete);
            if scenario != "success" {
                child.env("NGIT_TEST_EXPECT_FAILURE", "1");
            }
            let output = assert_cmd::Command::from_std(child)
                .timeout(Duration::from_secs(30))
                .output()
                .unwrap();
            assert!(output.status.success(), "{scenario}: {output:?}");
            for name in ["ngit", "git-remote-nostr", RECEIPT] {
                assert_eq!(
                    obsolete.join(name).exists(),
                    scenario != "success",
                    "{scenario}: {name}"
                );
                assert!(retained.join(name).exists());
            }
            assert!(obsolete.join("unrelated").exists());
        }
    }
}
