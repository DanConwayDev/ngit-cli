//! ngit-grasp subprocess fixture.
//!
//! Spawns the real `ngit-grasp` binary in in-memory test mode on a loopback
//! port and waits for it to start accepting connections. Modelled on
//! ngit-grasp's own `tests/common/relay.rs` — same env-var configuration,
//! same TCP-connect readiness probe, same drop-based shutdown.
//!
//! We deliberately do **not** link against `ngit_grasp` as a library: it pulls
//! in `clap`, `dotenvy`, `tracing-subscriber`, etc., none of which are wanted
//! in a test crate. Subprocess management is the same model used in
//! production and exercises the actual wire protocol.
//!
//! ## Binary discovery
//!
//! 1. `$NGIT_GRASP_BIN` — explicit override (preferred on CI).
//! 2. `<workspace_parent>/ngit-grasp/target/release/ngit-grasp` — convenient
//!    local-dev fallback for the sibling-clone layout assumed by the design
//!    doc.
//! 3. Fail with a clear error pointing at the design doc.
//!
//! ## Process lifecycle
//!
//! Subprocess startup uses [`std::process::Command`] — `wait_for_ready` uses
//! [`tokio::net::TcpStream`] so we don't block the test runtime. Shutdown
//! happens in `Drop`: `kill` + (blocking) `wait` to reap. The `wait` is
//! cheap because the process has already been signalled, but it does keep
//! drop synchronous, matching the ngit-grasp upstream pattern.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use tempfile::TempDir;
use tokio::time::sleep;

use crate::query;

/// How long to wait for ngit-grasp to accept TCP connections before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval for the readiness probe.
const READY_POLL: Duration = Duration::from_millis(100);
/// Tiny grace period after the first successful TCP connect, mirroring the
/// upstream pattern. The relay listener accepts before the websocket handler
/// is fully wired; without this the first REQ can race the binding.
const READY_GRACE: Duration = Duration::from_millis(100);

/// A spawned `ngit-grasp` subprocess plus the tempdir holding its git data.
///
/// Drop the value to terminate the subprocess and reclaim the tempdir.
#[derive(Debug)]
pub struct GraspServer {
    role: String,
    /// `http://127.0.0.1:<port>` — the form `Params::default()`'s
    /// `grasp_default_set` and `--grasp-server` flag both accept after
    /// passing through `normalize_grasp_server_url`.
    url: String,
    port: u16,
    process: Child,
    /// Held purely to keep the tempdir alive — `git_data_path` shares its
    /// lifetime.
    _git_data_dir: TempDir,
    git_data_path: PathBuf,
}

impl GraspServer {
    /// Spawn ngit-grasp bound to the given loopback port, wait for it to be
    /// ready, then return.
    pub(crate) async fn start(role: impl Into<String>, port: u16) -> Result<Self> {
        let role = role.into();
        let binary = locate_binary()?;

        let bind_address = format!("127.0.0.1:{port}");
        let url = format!("http://127.0.0.1:{port}");

        let git_data_dir =
            TempDir::new().context("failed to allocate tempdir for ngit-grasp git data")?;
        let git_data_path = git_data_dir.path().to_path_buf();

        // Match the upstream tests/common/relay.rs env set. The "_TEST" /
        // jitter / startup-delay knobs are what make ngit-grasp usable
        // synchronously inside a per-test fixture; without them the
        // background sync tasks can keep the process busy for seconds.
        let process = Command::new(&binary)
            // ngit-grasp writes `.relay-owner.nsec` into CWD on first start
            // if no key is supplied; point that at the tempdir so the file
            // is cleaned up with the rest of the fixture and doesn't litter
            // the test crate's working directory.
            .current_dir(&git_data_path)
            .env("NGIT_BIND_ADDRESS", &bind_address)
            .env("NGIT_DOMAIN", &bind_address)
            .env("NGIT_GIT_DATA_PATH", &git_data_path)
            .env("NGIT_DATABASE_BACKEND", "memory")
            .env("NGIT_TEST", "1")
            .env("NGIT_SYNC_STARTUP_DELAY_SECS", "0")
            .env("NGIT_SYNC_STARTUP_JITTER_MS", "0")
            .env("NGIT_SYNC_DISCONNECT_CHECK_INTERVAL_SECS", "1")
            // Detach from the test's stdio. Tests assert on the harness's
            // event store / git state, not on ngit-grasp logs — and noisy
            // INFO output makes `cargo test --nocapture` unreadable.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!("failed to spawn ngit-grasp binary at {}", binary.display())
            })?;

        let server = Self {
            role,
            url,
            port,
            process,
            _git_data_dir: git_data_dir,
            git_data_path,
        };
        server.wait_for_ready().await?;
        Ok(server)
    }

    /// Probe the listener with async TCP connects until it accepts. Mirrors
    /// the pattern in ngit-grasp's own test harness.
    async fn wait_for_ready(&self) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await {
                Ok(_) => {
                    sleep(READY_GRACE).await;
                    return Ok(());
                }
                Err(_) if Instant::now() < deadline => {
                    sleep(READY_POLL).await;
                }
                Err(e) => {
                    bail!(
                        "ngit-grasp at 127.0.0.1:{} did not become ready within {:?}: {e}",
                        self.port,
                        READY_TIMEOUT,
                    );
                }
            }
        }
    }

    /// Role label this server was registered under (e.g. `"repo"`).
    pub fn role(&self) -> &str {
        &self.role
    }

    /// `http://127.0.0.1:<port>` — accepted directly by ngit's
    /// `--grasp-server` flag and by `NGIT_GRASP_DEFAULT_SET`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// `ws://127.0.0.1:<port>` — the relay endpoint of the grasp service.
    /// Useful for tests that publish a vanilla event directly (rare; usually
    /// you want [`events`] instead).
    pub fn relay_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    /// Root path under which ngit-grasp creates bare repositories on receipt
    /// of an accepted kind-30617 announcement. Paths are laid out as
    /// `<root>/<npub>/<identifier>.git` (see ngit-grasp
    /// `nostr/policy/announcement.rs::ensure_bare_repository`).
    pub fn git_data_path(&self) -> &Path {
        &self.git_data_path
    }

    /// Query ngit-grasp's nostr surface over a real websocket REQ. Identical
    /// shape to [`crate::VanillaRelay::events`]; the wire path is the same,
    /// only the server implementation differs.
    pub async fn events(&self, filter: Filter) -> Result<Vec<Event>> {
        query::fetch_events(&self.relay_url(), filter).await
    }
}

impl Drop for GraspServer {
    fn drop(&mut self) {
        // Kill is best-effort — if the process already exited we don't care.
        // `wait` reaps the zombie; without it CI runs accumulate defunct
        // ngit-grasp entries.
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn locate_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("NGIT_GRASP_BIN") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "NGIT_GRASP_BIN points to `{p}` but no file exists there. \
             Build ngit-grasp or fix the path; see docs/architecture/test-harness.md."
        );
    }

    // Local-dev fallback:
    // `<workspace_parent>/ngit-grasp/target/release/ngit-grasp`.
    // `CARGO_MANIFEST_DIR` here is `<workspace_root>/test_harness`, so the
    // sibling ngit-grasp clone is two levels up + "ngit-grasp/...".
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .context("test_harness has no parent dir — corrupted layout?")?;
    let workspace_parent = workspace_root
        .parent()
        .context("workspace root has no parent dir — corrupted layout?")?;
    let sibling = workspace_parent.join("ngit-grasp/target/release/ngit-grasp");
    if sibling.exists() {
        return Ok(sibling);
    }

    bail!(
        "ngit-grasp binary not found. Either set NGIT_GRASP_BIN to the binary path, \
         or build a sibling clone at {} (cargo build --release inside ../ngit-grasp). \
         See docs/architecture/test-harness.md.",
        sibling.display()
    );
}
