//! `TempDir`-backed git repository fixture with env-pre-configured
//! `Command` wrappers for `ngit` and `git`.
//!
//! Every command spawned via [`Repo::ngit`] / [`Repo::git`] inherits the
//! harness env (`NGITTEST`, `NGIT_RELAY_*_SET`, ...) and runs with `cwd`
//! pointed at the repo's working tree. `PATH` is augmented so that `git`
//! can find `git-remote-nostr` for any test that exercises `nostr://`
//! remotes — even though the relay-only lighthouse does not need it.
//!
//! The repo is initialised with `main` as the default branch and a benign
//! `user.name` / `user.email` so commits succeed without touching the
//! caller's global git identity.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use tempfile::TempDir;

use crate::{harness::Harness, snapshot::RepoSnapshot};

/// One repo, owned for the lifetime of a test.
pub struct Repo {
    /// Owns the underlying tempdir; dropping `Repo` cleans the working tree.
    _tempdir: TempDir,
    dir: PathBuf,
    env: Vec<(String, String)>,
    ngit_bin: PathBuf,
    /// `PATH` augmented with the directory containing `git-remote-nostr`,
    /// so git can resolve the helper when spawning subprocesses.
    augmented_path: OsString,
}

impl Repo {
    pub(crate) fn init(harness: &Harness) -> Result<Self> {
        let tempdir = TempDir::new().context("failed to allocate TempDir for fresh_repo")?;
        let dir = tempdir.path().to_path_buf();

        // `git init -b main` works on git ≥ 2.28 (released 2020-07). The CI
        // baseline assumed by the project is well above this.
        let status = Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(&dir)
            .status()
            .context("failed to spawn git init")?;
        if !status.success() {
            anyhow::bail!("git init exited {status}");
        }

        // Benign per-repo identity so future `git commit`s don't trip the
        // default-identity check. Using --local keeps it scoped to this
        // tempdir — no contamination of the caller's global config.
        for (k, v) in [
            ("user.name", "ngit test"),
            ("user.email", "ngit-test@example.invalid"),
            ("commit.gpgSign", "false"),
        ] {
            let status = Command::new("git")
                .current_dir(&dir)
                .args(["config", "--local", k, v])
                .status()
                .with_context(|| format!("failed to git config {k}"))?;
            if !status.success() {
                anyhow::bail!("git config {k} exited {status}");
            }
        }

        // Augment PATH with the dir containing git-remote-nostr, so any
        // future `git clone nostr://...` driven from this repo finds the
        // helper.
        let helper_dir = harness
            .git_remote_nostr_bin()
            .parent()
            .context("git-remote-nostr binary has no parent dir")?
            .to_path_buf();
        let existing_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths: Vec<PathBuf> = std::env::split_paths(&existing_path).collect();
        if !paths.contains(&helper_dir) {
            paths.insert(0, helper_dir);
        }
        let augmented_path = std::env::join_paths(paths).context("failed to join PATH")?;

        Ok(Self {
            _tempdir: tempdir,
            dir,
            env: harness.env(),
            ngit_bin: harness.ngit_bin().to_path_buf(),
            augmented_path,
        })
    }

    /// Working tree path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Build a `Command` for the ngit binary under test. `cwd`, `env`, and
    /// `PATH` are all pre-configured; callers append args and call
    /// `.output()` / `.status()` themselves.
    pub fn ngit<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = Command::new(&self.ngit_bin);
        self.configure(&mut cmd);
        cmd.args(args);
        cmd
    }

    /// Build a `Command` for `git` with the harness env applied. Useful for
    /// driving `git clone`, `git push`, etc. against ngit's remote helper.
    pub fn git<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = Command::new("git");
        self.configure(&mut cmd);
        cmd.args(args);
        cmd
    }

    fn configure(&self, cmd: &mut Command) {
        cmd.current_dir(&self.dir);
        // env_clear() would also remove harmless inherited vars like HOME
        // that git needs; instead we just override the keys we care about.
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.env("PATH", &self.augmented_path);
        // Keep ngit deterministic in tests: no interactive prompts, no
        // global config interference.
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    }

    /// Read a single git-config key from this repo's local config.
    ///
    /// Returns `Ok(None)` when the key is unset, `Err` only on a hard
    /// failure (process spawn error, malformed output).
    pub fn config(&self, key: &str) -> Result<Option<String>> {
        let out = self
            .git(["config", "--local", "--get", key])
            .output()
            .with_context(|| format!("failed to spawn git config --get {key}"))?;
        if out.status.success() {
            let s = String::from_utf8(out.stdout)
                .with_context(|| format!("git config {key} returned non-utf8"))?;
            Ok(Some(s.trim_end_matches('\n').to_string()))
        } else if out.status.code() == Some(1) {
            // git config exits 1 when the key is unset — distinct from a
            // genuine error (exit 2+).
            Ok(None)
        } else {
            anyhow::bail!(
                "git config {key} exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// Capture the current `HEAD` and refs. Grows as migrated tests demand
    /// more fields.
    pub fn snapshot(&self) -> Result<RepoSnapshot> {
        RepoSnapshot::capture(&self.dir)
    }
}
