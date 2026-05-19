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

use crate::{
    port::{self, PortReservation},
    query,
};

/// How long to wait for ngit-grasp to accept TCP connections before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval for the readiness probe.
const READY_POLL: Duration = Duration::from_millis(100);
/// Tiny grace period after the first successful TCP connect, mirroring the
/// upstream pattern. The relay listener accepts before the websocket handler
/// is fully wired; without this the first REQ can race the binding.
const READY_GRACE: Duration = Duration::from_millis(100);
/// How many fresh port reservations to attempt before giving up. The
/// subprocess binds itself from `NGIT_BIND_ADDRESS`, so there is a
/// microsecond-scale TOCTOU window between [`PortReservation::release`]
/// and the subprocess's own `bind`. If that window loses the race the
/// subprocess exits before its TCP listener accepts, and our readiness
/// check picks that up via `try_wait` so we can retry on a fresh port
/// instead of hanging for the full 15s readiness timeout.
///
/// In practice this loop has never been observed to fire in local
/// stress testing — kept as defense-in-depth for CI / loaded hardware.
/// Matches the cap in `relay.rs`.
const MAX_BIND_ATTEMPTS: usize = 5;

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
    /// Spawn ngit-grasp on the port held by `reservation`, wait for it to
    /// be ready, then return.
    ///
    /// The reservation is released (its `TcpListener` dropped) immediately
    /// before `Command::spawn()`, so no other `reserve_port` call in this
    /// process can be handed the same port while ngit-grasp is starting
    /// up. There is a small TOCTOU window between release and the
    /// subprocess's own `bind` — passing a pre-bound fd into a subprocess
    /// would close it entirely, but isn't worth the Unix-specific
    /// `pre_exec` plumbing for a residual race that hasn't been observed
    /// in local stress testing.
    ///
    /// If the subprocess exits before becoming ready (the signature of
    /// having lost the bind race), we retry with a fresh reservation up
    /// to [`MAX_BIND_ATTEMPTS`] times. Defense-in-depth — never observed
    /// to fire locally; kept for CI / loaded hardware.
    pub(crate) async fn start(
        role: impl Into<String>,
        reservation: PortReservation,
    ) -> Result<Self> {
        let role = role.into();
        let binary = locate_binary()?;

        let mut reservation = Some(reservation);
        for attempt in 1..=MAX_BIND_ATTEMPTS {
            // Each attempt consumes the current reservation. On retry we
            // re-acquire from the kernel — which is guaranteed to give us
            // a port number different from any reservation currently held
            // elsewhere in this process.
            let r = reservation
                .take()
                .expect("reservation always present on attempt entry");
            match Self::try_start_once(role.clone(), &binary, r).await {
                Ok(server) => return Ok(server),
                Err(StartFailure::EarlyExit { status }) if attempt < MAX_BIND_ATTEMPTS => {
                    eprintln!(
                        "[test_harness] ngit-grasp exited early on attempt \
                         {attempt}/{MAX_BIND_ATTEMPTS} (status: {status:?}); \
                         likely a port-bind race — retrying with a fresh port",
                    );
                    reservation = Some(port::reserve_port().context(
                        "failed to reserve replacement port after ngit-grasp early exit",
                    )?);
                    continue;
                }
                Err(StartFailure::EarlyExit { status }) => {
                    bail!(
                        "ngit-grasp subprocess exited early after {MAX_BIND_ATTEMPTS} attempts \
                         (last exit status: {status:?}). If this is not a port-bind race, \
                         check ngit-grasp logs by temporarily enabling stderr in grasp.rs."
                    );
                }
                Err(StartFailure::Other(e)) => return Err(e),
            }
        }
        unreachable!("MAX_BIND_ATTEMPTS loop terminated without returning")
    }

    /// One attempt at spawning and waiting for ngit-grasp on the given
    /// reservation. Returns `Err(StartFailure::EarlyExit)` specifically
    /// when the subprocess died before becoming ready — the caller may
    /// retry in that case.
    async fn try_start_once(
        role: String,
        binary: &Path,
        reservation: PortReservation,
    ) -> std::result::Result<Self, StartFailure> {
        let port = reservation.port();
        let bind_address = format!("127.0.0.1:{port}");
        let url = format!("http://127.0.0.1:{port}");

        let git_data_dir = TempDir::new()
            .context("failed to allocate tempdir for ngit-grasp git data")
            .map_err(StartFailure::Other)?;
        let git_data_path = git_data_dir.path().to_path_buf();

        // Build the Command *before* releasing the reservation so that
        // none of the env-setting allocations happen while the listener
        // is held. We then release immediately before `spawn`.
        let mut cmd = Command::new(binary);
        cmd
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
            .stderr(Stdio::null());

        // Release the port reservation immediately before spawning the
        // subprocess that will bind it. Holding the reservation through
        // env-var setup above is what keeps any concurrent
        // `reserve_port()` calls from picking this same number.
        let _ = reservation.release();

        let process = cmd
            .spawn()
            .with_context(|| format!("failed to spawn ngit-grasp binary at {}", binary.display()))
            .map_err(StartFailure::Other)?;

        let mut server = Self {
            role,
            url,
            port,
            process,
            _git_data_dir: git_data_dir,
            git_data_path,
        };
        server.wait_for_ready_or_early_exit().await?;
        Ok(server)
    }

    /// Probe the listener with async TCP connects until it accepts, while
    /// concurrently watching for the subprocess to exit early (the
    /// signature of a bind-collision). Mirrors the pattern in ngit-grasp's
    /// own test harness, plus the early-exit detection that lets us retry
    /// instead of waiting the full 15s for a doomed process.
    async fn wait_for_ready_or_early_exit(&mut self) -> std::result::Result<(), StartFailure> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            // Check whether the subprocess has already exited. If so the
            // TCP probe will never succeed — bail immediately so the
            // caller can retry with a fresh port.
            match self.process.try_wait() {
                Ok(Some(status)) => return Err(StartFailure::EarlyExit { status }),
                Ok(None) => { /* still running */ }
                Err(e) => {
                    return Err(StartFailure::Other(anyhow::Error::from(e).context(
                        "failed to poll ngit-grasp subprocess status during readiness check",
                    )));
                }
            }

            match tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await {
                Ok(_) => {
                    sleep(READY_GRACE).await;
                    return Ok(());
                }
                Err(_) if Instant::now() < deadline => {
                    sleep(READY_POLL).await;
                }
                Err(e) => {
                    return Err(StartFailure::Other(anyhow::anyhow!(
                        "ngit-grasp at 127.0.0.1:{} did not become ready within {:?}: {e}",
                        self.port,
                        READY_TIMEOUT,
                    )));
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

/// Internal failure mode for a single [`GraspServer::try_start_once`] attempt.
///
/// `EarlyExit` is the retry-eligible case (subprocess died before
/// becoming ready — almost always a lost bind race); `Other` is anything
/// else and is propagated unchanged.
enum StartFailure {
    /// The subprocess exited before passing the readiness check.
    EarlyExit { status: std::process::ExitStatus },
    /// Any other error (tempdir, spawn, IO during probe, etc.). Propagated
    /// to the caller without retry.
    Other(anyhow::Error),
}

impl From<StartFailure> for anyhow::Error {
    fn from(value: StartFailure) -> Self {
        match value {
            StartFailure::EarlyExit { status } => anyhow::anyhow!(
                "ngit-grasp subprocess exited before becoming ready (status: {status:?})"
            ),
            StartFailure::Other(e) => e,
        }
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
