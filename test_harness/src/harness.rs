//! Harness builder and top-level fixture.
//!
//! `Harness` owns the relay + grasp roster and the binary paths needed to
//! spawn ngit / git commands; it hands out per-test [`Repo`] fixtures via
//! [`Harness::fresh_repo`]. Drop the `Harness` to shut everything down —
//! vanilla relays are `Drop`-managed inside their wrappers, grasp servers
//! have an explicit `Drop` that kills the subprocess, and `TempDir`s clean
//! themselves up.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};

use crate::{grasp::GraspServer, port, relay::VanillaRelay, repo::Repo};

/// Per-role relay + grasp roster + injected env, ready to drive ngit
/// subprocesses.
pub struct Harness {
    /// Loopback vanilla relays keyed by role label. A role may carry multiple
    /// relays; they aggregate into a `;`-separated env-var value.
    relays: BTreeMap<String, Vec<VanillaRelay>>,
    /// `ngit-grasp` subprocesses keyed by role label. Aggregation rules
    /// match `relays`. Roles `"repo"` (or any unrecognised label) feed into
    /// `NGIT_GRASP_DEFAULT_SET`.
    grasps: BTreeMap<String, Vec<GraspServer>>,
    /// Absolute path to the `ngit` binary under test.
    ngit_bin: PathBuf,
    /// Absolute path to the `git-remote-nostr` binary under test. Stored
    /// even though the relay-only lighthouse does not need it — every other
    /// migrated test will, and resolving it once is cheaper than threading
    /// it through later.
    git_remote_nostr_bin: PathBuf,
}

impl Harness {
    /// Fluent entry point. Both binary paths are required up front so the
    /// harness can fail fast if cargo did not produce them.
    pub fn builder(
        ngit_bin: impl Into<PathBuf>,
        git_remote_nostr_bin: impl Into<PathBuf>,
    ) -> HarnessBuilder {
        HarnessBuilder {
            relay_roles: Vec::new(),
            grasp_roles: Vec::new(),
            ngit_bin: ngit_bin.into(),
            git_remote_nostr_bin: git_remote_nostr_bin.into(),
        }
    }

    /// Look up a relay by role. Panics if the role has no relays — caller
    /// bug, not a test failure mode.
    pub fn relay(&self, role: &str) -> &VanillaRelay {
        self.relays
            .get(role)
            .and_then(|v| v.first())
            .unwrap_or_else(|| panic!("no relay registered under role {role:?}"))
    }

    /// All relays for a role (multiple `with_relay("default")` calls).
    pub fn relays(&self, role: &str) -> &[VanillaRelay] {
        self.relays.get(role).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Look up a grasp server by role. Panics if the role has no grasp
    /// servers — caller bug, not a test failure mode.
    pub fn grasp(&self, role: &str) -> &GraspServer {
        self.grasps
            .get(role)
            .and_then(|v| v.first())
            .unwrap_or_else(|| panic!("no grasp server registered under role {role:?}"))
    }

    /// All grasp servers for a role.
    pub fn grasps(&self, role: &str) -> &[GraspServer] {
        self.grasps.get(role).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// `;`-separated URL list for the given relay role — the format every
    /// `NGIT_RELAY_*` env var expects.
    fn relay_role_urls(&self, role: &str) -> String {
        self.relays(role)
            .iter()
            .map(|r| r.url().to_string())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// `;`-separated `http://host:port` list spanning **every** registered
    /// grasp server, regardless of role label. `NGIT_GRASP_DEFAULT_SET` is
    /// the only env var ngit reads for grasp fallback selection, so all
    /// roles collapse into it.
    fn all_grasp_urls(&self) -> String {
        self.grasps
            .values()
            .flat_map(|v| v.iter().map(|g| g.url().to_string()))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Env-var pairs to inject into every spawned child. Includes
    /// `NGITTEST=TRUE` so `Params::default()` enters its test branch, plus
    /// each `NGIT_*_SET` populated from the corresponding role.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![("NGITTEST".to_string(), "TRUE".to_string())];

        let default_urls = self.relay_role_urls("default");
        if !default_urls.is_empty() {
            env.push(("NGIT_RELAY_DEFAULT_SET".to_string(), default_urls));
        }
        let blaster_urls = self.relay_role_urls("blaster");
        if !blaster_urls.is_empty() {
            env.push(("NGIT_RELAY_BLASTER_SET".to_string(), blaster_urls));
        }
        let signer_urls = self.relay_role_urls("signer_fallback");
        if !signer_urls.is_empty() {
            env.push(("NGIT_RELAY_SIGNER_FALLBACK_SET".to_string(), signer_urls));
        }
        let grasp_urls = self.all_grasp_urls();
        if !grasp_urls.is_empty() {
            env.push(("NGIT_GRASP_DEFAULT_SET".to_string(), grasp_urls));
        }

        env
    }

    /// Absolute path to the `ngit` binary the test will spawn.
    pub fn ngit_bin(&self) -> &Path {
        &self.ngit_bin
    }

    /// Absolute path to the `git-remote-nostr` binary. Tests that exercise
    /// `nostr://` remotes need this on `PATH` (or in `GIT_EXEC_PATH`) for
    /// git to discover the helper.
    pub fn git_remote_nostr_bin(&self) -> &Path {
        &self.git_remote_nostr_bin
    }

    /// Mint a fresh `TempDir`-backed git repo configured with the harness
    /// env. The repo is `git init`'d with `main` as the default branch and
    /// has a benign `user.name` / `user.email` so commits don't trip the
    /// default git identity check.
    pub fn fresh_repo(&self) -> Result<Repo> {
        Repo::init(self)
    }
}

/// Fluent builder for [`Harness`].
pub struct HarnessBuilder {
    relay_roles: Vec<String>,
    grasp_roles: Vec<String>,
    ngit_bin: PathBuf,
    git_remote_nostr_bin: PathBuf,
}

impl HarnessBuilder {
    /// Register a vanilla nostr relay under the given role label.
    ///
    /// Standard roles consumed by `Params::default()`:
    /// `"default"`, `"blaster"`, `"signer_fallback"`. Other labels are
    /// accepted but won't be injected into ngit's env — useful for tests
    /// that publish to a relay ngit shouldn't know about.
    pub fn with_relay(mut self, role: impl Into<String>) -> Self {
        self.relay_roles.push(role.into());
        self
    }

    /// Register a real `ngit-grasp` subprocess under the given role label.
    ///
    /// Every registered grasp server feeds into `NGIT_GRASP_DEFAULT_SET`
    /// regardless of role — the role label is purely for the test's own
    /// look-ups via `Harness::grasp(role)`.
    pub fn with_grasp_server(mut self, role: impl Into<String>) -> Self {
        self.grasp_roles.push(role.into());
        self
    }

    /// Build the harness: allocate ports, start every relay and grasp
    /// subprocess, then return when all are accepting connections.
    pub async fn build(self) -> Result<Harness> {
        if !self.ngit_bin.exists() {
            return Err(anyhow!(
                "ngit binary not found at {} — did `cargo test` build it?",
                self.ngit_bin.display()
            ));
        }
        if !self.git_remote_nostr_bin.exists() {
            return Err(anyhow!(
                "git-remote-nostr binary not found at {} — did `cargo test` build it?",
                self.git_remote_nostr_bin.display()
            ));
        }

        let mut relays: BTreeMap<String, Vec<VanillaRelay>> = BTreeMap::new();
        for role in self.relay_roles {
            let port = port::find_free_port()
                .with_context(|| format!("failed to allocate port for relay role {role:?}"))?;
            let relay = VanillaRelay::start(role.clone(), port)
                .await
                .with_context(|| format!("failed to start relay for role {role:?}"))?;
            relays.entry(role).or_default().push(relay);
        }

        let mut grasps: BTreeMap<String, Vec<GraspServer>> = BTreeMap::new();
        for role in self.grasp_roles {
            let port = port::find_free_port()
                .with_context(|| format!("failed to allocate port for grasp role {role:?}"))?;
            let server = GraspServer::start(role.clone(), port)
                .await
                .with_context(|| format!("failed to start ngit-grasp for role {role:?}"))?;
            grasps.entry(role).or_default().push(server);
        }

        Ok(Harness {
            relays,
            grasps,
            ngit_bin: self.ngit_bin,
            git_remote_nostr_bin: self.git_remote_nostr_bin,
        })
    }
}
