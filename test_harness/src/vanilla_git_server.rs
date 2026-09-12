//! Vanilla (non-grasp) Smart-HTTP git server fixture.
//!
//! Spawns an in-process HTTP server that speaks the Git Smart HTTP protocol
//! by execing `git upload-pack` / `git receive-pack` subprocesses against a
//! bare clone of a source repository. Both fetch and push are supported.
//!
//! ## Why this exists
//!
//! ngit's repo-announcement events list clone URLs that may point at either
//! a grasp server or a plain git host. The two paths diverge throughout the
//! codebase — `is_grasp_server_clone_url`, `is_grasp_server_in_list`,
//! identifier inference in `repo_ref.rs`, push fan-out in `push.rs`. The
//! [`crate::GraspServer`] fixture covers the grasp branch; this fixture
//! covers the non-grasp branch with the cheapest possible "just a git
//! server" implementation.
//!
//! ## Two entry points
//!
//! - [`VanillaGitServer::start`] — front a bare clone of an existing
//!   `source_repo`. Used when a test wants pre-populated content on the server.
//! - [`VanillaGitServer::start_empty`] — front a freshly initialised empty bare
//!   repo (`git init --bare -b main`). Used by the [`crate::Harness`]
//!   integration where the server is registered at builder time, before any
//!   test repo exists. Tests that need content push it explicitly.
//!
//! ## Why not link the upstream version
//!
//! ngit-grasp has a near-identical `SimpleGitServer` / `SmartGitServer`
//! pair in its `tests/common/` tree. That module is private to ngit-grasp's
//! own tests and not exposed as a library, so we vendor the smart-HTTP
//! variant here with three differences worth noting up front:
//!
//! 1. **Push works.** The upstream version returns 403 for `git-receive-pack`;
//!    we implement the symmetric handler and accept pushes by default.
//!    Configured to allow deletes and non-fast-forwards so force-push and
//!    delete-branch tests behave as the test author expects.
//! 2. **Hooks are stripped after clone.** `git clone --bare` copies the source
//!    repo's `hooks/` directory; an over-zealous `pre-receive` would reject
//!    every push. We wipe `hooks/` after the clone — tests can install their
//!    own hook content into [`Self::repo_path`] if they need one.
//! 3. **Drop joins the accept task.** Best-effort blocking shutdown via
//!    `tokio::task::block_in_place` + `Handle::block_on`, matching the
//!    `GraspServer` reaping pattern. Avoids zombie listener tasks accumulating
//!    across a large parallel test run.
//!
//! ## Port management
//!
//! Uses the same [`crate::port::reserve_port`] pattern as every other
//! fixture in this crate. The accept loop binds the listener directly
//! from the reservation's underlying fd (via `TcpListener::from_std`),
//! so there is **zero** TOCTOU window — no other fixture in this
//! process can be handed this port number between reservation and bind.
//!
//! ## Runtime requirement: multi_thread tokio
//!
//! This fixture must be driven from a multi-thread tokio runtime
//! (`#[tokio::test(flavor = "multi_thread")]` or `Runtime` configured
//! likewise). The accept loop is a spawned task; if the calling test
//! issues a synchronous `std::process::Command::output()` for a `git
//! push` (or any other operation that goes back through this server)
//! on a single-threaded runtime, the test thread blocks inside `output`
//! and the accept task never gets a chance to handle the incoming
//! request — instant deadlock.
//!
//! The harness's existing async fixtures (`VanillaRelay`, `GraspServer`)
//! happen to tolerate `current_thread` because their wire path is
//! exercised exclusively through `nostr-sdk` futures that yield
//! correctly. `VanillaGitServer` cannot make the same assumption —
//! `git push` from `std::process` is the canonical test workload.

use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tempfile::TempDir;
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    process::Command as TokioCommand,
    sync::oneshot,
    task::{JoinHandle, JoinSet},
};

use crate::port::PortReservation;

/// A spawned smart-HTTP git server fronting a tempdir-backed bare repo.
///
/// Drop the value to shut the listener down and reclaim the tempdir.
#[derive(Debug)]
pub struct VanillaGitServer {
    role: String,
    /// `http://127.0.0.1:<port>` — the URL form accepted by `git clone`,
    /// `git fetch`, `git push`, `ls-remote`, and ngit's clone-URL list.
    url: String,
    #[allow(dead_code)]
    port: u16,
    /// Bare repo on disk. Lives inside `_temp_dir` — exposed so tests
    /// can install hooks, inspect refs out-of-band, etc.
    repo_path: PathBuf,
    /// Count of HTTP requests carrying ngit's NIP-98 authorization scheme.
    nostr_authorization_requests: Arc<AtomicUsize>,
    /// Shutdown signal sender. `take()`n in `Drop` (and consumed by `stop`).
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Accept loop join handle. Awaited by `stop`, aborted by `Drop`.
    handle: Option<JoinHandle<()>>,
    /// Holds the bare repo. Cleaned up on drop.
    _temp_dir: TempDir,
}

impl VanillaGitServer {
    /// Start a vanilla git server fronting a fresh bare clone of
    /// `source_repo`. The port held by `reservation` is what the server
    /// binds.
    ///
    /// `source_repo` may be bare or non-bare; the server always works
    /// against an internal bare clone in a tempdir, so the source is
    /// snapshotted at start time and not mutated by subsequent pushes
    /// (mutations land on the bare clone instead).
    pub async fn start(
        role: impl Into<String>,
        reservation: PortReservation,
        source_repo: &Path,
    ) -> Result<Self> {
        // Prepare the bare clone *before* releasing the reservation by
        // way of `into_std_listener` (which happens inside `bootstrap`).
        // This keeps the port held while we do the slowest part of
        // startup, which means concurrent `reserve_port` calls still
        // can't be handed this number.
        let temp_dir =
            TempDir::new().context("failed to allocate tempdir for VanillaGitServer bare repo")?;
        let repo_path = temp_dir.path().join("repo.git");

        let output = Command::new("git")
            .args(["clone", "--bare"])
            .arg(source_repo)
            .arg(&repo_path)
            .output()
            .context("failed to invoke git clone --bare")?;
        if !output.status.success() {
            bail!(
                "git clone --bare failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Self::bootstrap(role, reservation, temp_dir, repo_path).await
    }

    /// Start a vanilla git server fronting a freshly initialised, empty
    /// bare repository — `git init --bare -b main` inside a harness-owned
    /// tempdir. The repo has zero refs at startup, so `git ls-remote`
    /// against the returned URL succeeds with empty output (a clean
    /// liveness check that doesn't smuggle ref names into a test's
    /// assumptions).
    ///
    /// Intended for the [`crate::Harness`] integration where the server
    /// is registered at builder time, before any test repo exists. Tests
    /// that need pre-populated content either:
    ///
    /// - push their own commits to the server with `git push <Self::url()>
    ///   <branch>` from a test-authored working tree, then continue, or
    /// - construct a sourced server explicitly via [`Self::start`].
    ///
    /// Same port-reservation, hook-stripping, and runtime requirements
    /// as [`Self::start`] — see the module-level docs.
    pub async fn start_empty(
        role: impl Into<String>,
        reservation: PortReservation,
    ) -> Result<Self> {
        let temp_dir = TempDir::new()
            .context("failed to allocate tempdir for empty VanillaGitServer bare repo")?;
        let repo_path = temp_dir.path().join("repo.git");

        // `-b main` (initial-branch) sets HEAD to `ref: refs/heads/main`
        // even though that ref doesn't exist yet, matching what a
        // freshly init'd source repo would look like. Tests that push to
        // the server with `main:main` therefore end up with the same
        // ref layout they'd get against any real-world git host.
        let output = Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&repo_path)
            .output()
            .context("failed to invoke git init --bare")?;
        if !output.status.success() {
            bail!(
                "git init --bare failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Self::bootstrap(role, reservation, temp_dir, repo_path).await
    }

    /// Shared tail of [`Self::start`] / [`Self::start_empty`]: strip the
    /// hooks dir, run `update-server-info`, promote the reservation's
    /// listener into a tokio accept loop, and probe for readiness.
    ///
    /// `repo_path` must already exist and be a valid bare repo (whatever
    /// produced it — `clone --bare` or `init --bare`).
    async fn bootstrap(
        role: impl Into<String>,
        reservation: PortReservation,
        temp_dir: TempDir,
        repo_path: PathBuf,
    ) -> Result<Self> {
        // Fail loudly on a current_thread runtime rather than deadlocking
        // when a test issues a blocking `git push` against this server. See
        // the "Runtime requirement: multi_thread tokio" section of the
        // module-level docs for the full reasoning. The default
        // `#[tokio::test]` flavor *is* current_thread, so this is the
        // failure mode most test authors will hit if they forget the
        // `flavor = "multi_thread"` attribute.
        let flavor = tokio::runtime::Handle::current().runtime_flavor();
        if matches!(flavor, tokio::runtime::RuntimeFlavor::CurrentThread) {
            bail!(
                "VanillaGitServer requires a multi-thread tokio runtime; \
                 annotate the test with #[tokio::test(flavor = \"multi_thread\")]. \
                 See test_harness/src/vanilla_git_server.rs § \"Runtime requirement\"."
            );
        }

        let role = role.into();
        let port = reservation.port();

        // Strip any hooks the bare repo carries. `clone --bare` copies
        // the source repo's hooks (an over-zealous `pre-receive` would
        // reject every push); `init --bare` only ships disabled
        // `.sample` files which are harmless but pointless. Either way,
        // tests that need a hook installed do so explicitly via
        // `Self::repo_path()`.
        let hooks_dir = repo_path.join("hooks");
        if hooks_dir.exists() {
            std::fs::remove_dir_all(&hooks_dir).context("failed to strip hooks/ from bare repo")?;
            // git complains on some platforms if hooks/ is missing entirely;
            // recreate it empty so the dir is present but no hooks are installed.
            std::fs::create_dir(&hooks_dir).context("failed to recreate empty hooks/ dir")?;
        }

        // Run update-server-info for completeness. We don't serve the dumb
        // protocol — kept because some test scripts inspect `info/refs`
        // directly out of habit and it's a no-op cost-wise.
        let output = Command::new("git")
            .args(["update-server-info"])
            .current_dir(&repo_path)
            .output()
            .context("failed to invoke git update-server-info")?;
        if !output.status.success() {
            bail!(
                "git update-server-info failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Hand the kernel-assigned fd straight into the accept loop. This
        // is the zero-TOCTOU path: we never release the port number back
        // to the OS between reservation and bind — same fd, just promoted
        // from `std` to `tokio`.
        let std_listener = reservation.into_std_listener();
        std_listener
            .set_nonblocking(true)
            .context("failed to set listener non-blocking")?;
        let listener = TcpListener::from_std(std_listener)
            .context("failed to promote std TcpListener into tokio TcpListener")?;

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let serve_repo = Arc::new(repo_path.clone());
        let nostr_authorization_requests = Arc::new(AtomicUsize::new(0));
        let serve_nostr_authorization_requests = Arc::clone(&nostr_authorization_requests);

        let handle: JoinHandle<()> = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, _addr)) => {
                                let repo = Arc::clone(&serve_repo);
                                let nostr_authorization_requests =
                                    Arc::clone(&serve_nostr_authorization_requests);
                                let io = TokioIo::new(stream);
                                connections.spawn(async move {
                                    let service = service_fn(move |req| {
                                        let repo = Arc::clone(&repo);
                                        let nostr_authorization_requests =
                                            Arc::clone(&nostr_authorization_requests);
                                        async move {
                                            handle_request(
                                                req,
                                                &repo,
                                                &nostr_authorization_requests,
                                            )
                                            .await
                                        }
                                    });
                                    if let Err(e) = http1::Builder::new()
                                        .serve_connection(io, service)
                                        .await
                                    {
                                        let msg = e.to_string();
                                        // Connection errors during client
                                        // disconnect are routine; suppress
                                        // the noise but report anything else.
                                        if !msg.contains("connection")
                                            && !msg.contains("IncompleteMessage")
                                        {
                                            eprintln!(
                                                "[VanillaGitServer] connection error: {e}"
                                            );
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                eprintln!("[VanillaGitServer] accept error: {e}");
                            }
                        }
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    _ = &mut shutdown_rx => break,
                }
            }
            connections.shutdown().await;
        });

        let url = format!("http://127.0.0.1:{port}");

        // The listener has remained bound since reservation. Requests can
        // queue immediately; the runtime drives the accept loop when polled.
        // Synchronous Git clients still require another runtime worker, as
        // described in this module's documentation.

        Ok(Self {
            role,
            url,
            port,
            repo_path,
            nostr_authorization_requests,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
            _temp_dir: temp_dir,
        })
    }

    /// Role label this server was registered under. Free-form —
    /// `VanillaGitServer` instances are not (currently) aggregated by role
    /// inside the harness, unlike `VanillaRelay` / `GraspServer`.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// `http://127.0.0.1:<port>` — what `git clone`, `git push`, ngit's
    /// announcement clone-URL list, etc. expect.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// On-disk bare repo backing this server. Tests can read refs
    /// directly (via `git -C <path>` or `git2`) to assert on the result
    /// of a push without going back over HTTP.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Number of requests that carried an `Authorization: Nostr ...` header.
    /// Public Git-host tests use this to prove ngit did not manufacture
    /// private-repository credentials for a public mirror.
    pub fn nostr_authorization_requests(&self) -> usize {
        self.nostr_authorization_requests.load(Ordering::SeqCst)
    }

    /// Read the OID that `refs/nostr/<event_id_hex>` resolves to inside this
    /// server's bare repository.
    ///
    /// Mirrors [`crate::GraspServer::read_nostr_ref`] for the vanilla
    /// (non-grasp) git server case, where the bare repo is `repo_path()`
    /// directly with no npub/identifier subdirectory.
    ///
    /// Returns an error if the ref does not exist — that is a genuine test
    /// failure: the PR push did not reach this server.
    pub async fn read_nostr_ref(&self, event_id_hex: &str) -> Result<String> {
        let refname = format!("refs/nostr/{event_id_hex}");
        let out = tokio::process::Command::new("git")
            .arg("for-each-ref")
            .arg(&refname)
            .arg("--format=%(objectname)")
            .current_dir(&self.repo_path)
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to spawn `git for-each-ref {refname}` in {}",
                    self.repo_path.display()
                )
            })?;
        if !out.status.success() {
            bail!(
                "`git for-each-ref {refname}` exited non-zero in {}: {}",
                self.repo_path.display(),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        let oid = String::from_utf8(out.stdout)
            .context("git for-each-ref output is not valid UTF-8")?
            .trim()
            .to_string();
        if oid.is_empty() {
            bail!(
                "ref {refname} not found in bare repo at {} — the push did not land",
                self.repo_path.display(),
            );
        }
        Ok(oid)
    }

    /// Explicit shutdown. Equivalent to dropping, but `await`able and
    /// waits for connection tasks to be cancelled. Prefer drop in tests.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for VanillaGitServer {
    fn drop(&mut self) {
        // Signal the accept loop to exit. The task itself may already be
        // gone (panic, runtime shutdown) — best effort.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Aborting the accept task drops its JoinSet, cancelling every
        // connection and its kill-on-drop Git child. `stop` also waits for
        // connection cancellation before returning.
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Top-level HTTP dispatch.
async fn handle_request(
    req: Request<Incoming>,
    repo_path: &Path,
    nostr_authorization_requests: &AtomicUsize,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req
        .headers()
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_whitespace().next())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Nostr"))
    {
        nostr_authorization_requests.fetch_add(1, Ordering::SeqCst);
    }
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let method = req.method().clone();
    let git_protocol = req
        .headers()
        .get("Git-Protocol")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // GET /info/refs?service=git-upload-pack | git-receive-pack
    if method == Method::GET && path.ends_with("/info/refs") {
        let service = query.split('&').find_map(|param| {
            let mut parts = param.splitn(2, '=');
            match (parts.next(), parts.next()) {
                (Some("service"), Some(svc)) => Some(svc.to_string()),
                _ => None,
            }
        });
        return match service.as_deref() {
            Some("git-upload-pack") => {
                Ok(handle_info_refs(repo_path, Service::UploadPack, git_protocol.as_deref()).await)
            }
            Some("git-receive-pack") => {
                Ok(
                    handle_info_refs(repo_path, Service::ReceivePack, git_protocol.as_deref())
                        .await,
                )
            }
            _ => Ok(plain_response(
                StatusCode::BAD_REQUEST,
                "missing or invalid service parameter",
            )),
        };
    }

    // POST /git-upload-pack | /git-receive-pack
    if method == Method::POST {
        if path.ends_with("/git-upload-pack") {
            return Ok(
                handle_rpc(req, repo_path, Service::UploadPack, git_protocol.as_deref()).await,
            );
        }
        if path.ends_with("/git-receive-pack") {
            return Ok(handle_rpc(
                req,
                repo_path,
                Service::ReceivePack,
                git_protocol.as_deref(),
            )
            .await);
        }
    }

    Ok(plain_response(StatusCode::NOT_FOUND, "not found"))
}

#[derive(Copy, Clone)]
enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    fn git_subcommand(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }

    fn service_name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    fn advertise_content_type(self) -> &'static str {
        match self {
            Self::UploadPack => "application/x-git-upload-pack-advertisement",
            Self::ReceivePack => "application/x-git-receive-pack-advertisement",
        }
    }

    fn result_content_type(self) -> &'static str {
        match self {
            Self::UploadPack => "application/x-git-upload-pack-result",
            Self::ReceivePack => "application/x-git-receive-pack-result",
        }
    }
}

/// Build the `git -c ... <subcommand>` invocation. The `-c` flags differ
/// per service: upload-pack gets the SHA1-want allowances commonly needed
/// by shallow / partial clones; receive-pack gets the permissive flags
/// that let force-push and delete-branch tests behave the way the test
/// author would assume on a vanilla test server.
fn build_git_command(service: Service) -> TokioCommand {
    let mut cmd = TokioCommand::new("git");
    cmd.kill_on_drop(true);
    match service {
        Service::UploadPack => {
            cmd.arg("-c")
                .arg("uploadpack.allowReachableSHA1InWant=true")
                .arg("-c")
                .arg("uploadpack.allowTipSHA1InWant=true")
                .arg("-c")
                .arg("uploadpack.allowFilter=true");
        }
        Service::ReceivePack => {
            cmd.arg("-c")
                .arg("receive.denyDeletes=false")
                .arg("-c")
                .arg("receive.denyNonFastForwards=false")
                .arg("-c")
                .arg("receive.denyCurrentBranch=ignore");
        }
    }
    cmd
}

/// `GET /info/refs?service=...` — ref advertisement.
async fn handle_info_refs(
    repo_path: &Path,
    service: Service,
    git_protocol_version: Option<&str>,
) -> Response<Full<Bytes>> {
    use std::process::Stdio;

    let mut cmd = build_git_command(service);
    cmd.arg(service.git_subcommand())
        .arg("--advertise-refs")
        .arg("--stateless-rpc");
    if let Some(version) = git_protocol_version {
        cmd.env("GIT_PROTOCOL", version);
    }
    cmd.arg(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[VanillaGitServer] spawn {} failed: {e}",
                service.git_subcommand()
            );
            return plain_response(StatusCode::INTERNAL_SERVER_ERROR, "spawn failed");
        }
    };

    let output = match child.wait_with_output().await {
        Ok(output) => output,
        Err(error) => {
            eprintln!("[VanillaGitServer] collecting advertisement failed: {error}");
            return plain_response(StatusCode::INTERNAL_SERVER_ERROR, "git IO failed");
        }
    };
    if !output.status.success() {
        eprintln!(
            "[VanillaGitServer] advertisement failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Smart-HTTP advertisement framing: pkt-line("# service=<name>\n"),
    // flush, then the upstream tool's stdout.
    let mut body = Vec::new();
    let service_line = format!("# service={}\n", service.service_name());
    let header_len = service_line.len() + 4;
    body.extend_from_slice(format!("{header_len:04x}").as_bytes());
    body.extend_from_slice(service_line.as_bytes());
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&output.stdout);

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", service.advertise_content_type())
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// `POST /git-upload-pack` or `POST /git-receive-pack`.
async fn handle_rpc(
    req: Request<Incoming>,
    repo_path: &Path,
    service: Service,
    git_protocol_version: Option<&str>,
) -> Response<Full<Bytes>> {
    use std::process::Stdio;

    let body_bytes = match req.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            eprintln!("[VanillaGitServer] failed to read request body: {e}");
            return plain_response(StatusCode::BAD_REQUEST, "bad body");
        }
    };

    let mut cmd = build_git_command(service);
    cmd.arg(service.git_subcommand()).arg("--stateless-rpc");
    if let Some(version) = git_protocol_version {
        cmd.env("GIT_PROTOCOL", version);
    }
    cmd.arg(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[VanillaGitServer] spawn {} failed: {e}",
                service.git_subcommand()
            );
            return plain_response(StatusCode::INTERNAL_SERVER_ERROR, "spawn failed");
        }
    };

    let output = match collect_git_output(child, &body_bytes).await {
        Ok(output) => output,
        Err(error) => {
            eprintln!("[VanillaGitServer] collecting RPC response failed: {error}");
            return plain_response(StatusCode::INTERNAL_SERVER_ERROR, "git IO failed");
        }
    };
    if !output.status.success() {
        eprintln!(
            "[VanillaGitServer] RPC failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", service.result_content_type())
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(output.stdout)))
        .unwrap()
}

/// Feed input while draining both output pipes so neither pipe can block Git.
/// Cancelling this future drops the child, whose command enables kill-on-drop.
async fn collect_git_output(
    mut child: tokio::process::Child,
    input: &[u8],
) -> std::io::Result<std::process::Output> {
    let stdin = child.stdin.take();
    let write = async {
        if let Some(mut stdin) = stdin {
            stdin.write_all(input).await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let (_, output) = tokio::try_join!(write, child.wait_with_output())?;
    Ok(output)
}

fn plain_response(status: StatusCode, msg: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(msg)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::port::reserve_port;

    /// Initialise a non-bare source repo with one commit on `main`. Returns
    /// the tempdir (kept alive by the caller) and the head commit oid.
    fn init_source_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        run_git(&dir, &["init", "-b", "main"]);
        run_git(&dir, &["config", "user.email", "harness@example.com"]);
        run_git(&dir, &["config", "user.name", "Harness"]);
        // Disable any inherited commit signing from the user's global git
        // config. Without this, `git commit` blocks on gpg-agent (and
        // potentially on a hardware token) which deadlocks the test.
        run_git(&dir, &["config", "commit.gpgSign", "false"]);
        run_git(&dir, &["config", "tag.gpgSign", "false"]);
        std::fs::write(dir.path().join("README.md"), "# source\n").unwrap();
        run_git(&dir, &["add", "README.md"]);
        run_git(&dir, &["commit", "-m", "initial", "--no-gpg-sign"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let head = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (dir, head)
    }

    fn run_git(dir: &TempDir, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ls_remote_returns_head() {
        let (src, head) = init_source_repo();
        let server = VanillaGitServer::start("test", reserve_port().unwrap(), src.path())
            .await
            .unwrap();
        let out = tokio::process::Command::new("git")
            .args(["ls-remote", server.url()])
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "ls-remote failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(&head), "ls-remote missing head: {stdout}");
        assert!(
            stdout.contains("refs/heads/main"),
            "ls-remote missing main: {stdout}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clone_then_push_then_reclone_sees_new_commit() {
        let (src, _head) = init_source_repo();
        let server = VanillaGitServer::start("test", reserve_port().unwrap(), src.path())
            .await
            .unwrap();

        // 1. clone the server into a working copy
        let work = TempDir::new().unwrap();
        let clone_out = tokio::process::Command::new("git")
            .args(["clone", server.url(), work.path().to_str().unwrap()])
            .output()
            .await
            .unwrap();
        assert!(
            clone_out.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&clone_out.stderr)
        );

        // 2. commit on a new branch and push it
        run_git_in(
            work.path(),
            &["config", "user.email", "harness@example.com"],
        );
        run_git_in(work.path(), &["config", "user.name", "Harness"]);
        run_git_in(work.path(), &["config", "commit.gpgSign", "false"]);
        run_git_in(work.path(), &["checkout", "-b", "feature"]);
        std::fs::write(work.path().join("new.txt"), "added\n").unwrap();
        run_git_in(work.path(), &["add", "new.txt"]);
        run_git_in(
            work.path(),
            &["commit", "-m", "add new.txt", "--no-gpg-sign"],
        );
        let push_out = std::process::Command::new("git")
            .args(["push", "origin", "feature"])
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            push_out.status.success(),
            "push failed: {}",
            String::from_utf8_lossy(&push_out.stderr)
        );

        // 3. fresh clone and verify the pushed branch is visible with the same tip
        let work_tip = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(work.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let second = TempDir::new().unwrap();
        let second_clone = tokio::process::Command::new("git")
            .args(["clone", server.url(), second.path().to_str().unwrap()])
            .output()
            .await
            .unwrap();
        assert!(
            second_clone.status.success(),
            "second clone failed: {}",
            String::from_utf8_lossy(&second_clone.stderr)
        );
        let ls = std::process::Command::new("git")
            .args(["rev-parse", "origin/feature"])
            .current_dir(second.path())
            .output()
            .unwrap();
        assert!(
            ls.status.success(),
            "rev-parse origin/feature failed: {}",
            String::from_utf8_lossy(&ls.stderr)
        );
        let pushed_tip = String::from_utf8(ls.stdout).unwrap().trim().to_string();
        assert_eq!(
            pushed_tip, work_tip,
            "pushed tip should be visible on reclone"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_push_and_delete_are_permitted() {
        // Vanilla git hosts vary on whether they allow non-FF and deletes;
        // for test purposes we want them on so test authors don't have to
        // think about it. This test pins that policy choice.
        let (src, _head) = init_source_repo();
        let server = VanillaGitServer::start("test", reserve_port().unwrap(), src.path())
            .await
            .unwrap();

        let work = TempDir::new().unwrap();
        let out = tokio::process::Command::new("git")
            .args(["clone", server.url(), work.path().to_str().unwrap()])
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
        run_git_in(work.path(), &["config", "user.email", "h@e.com"]);
        run_git_in(work.path(), &["config", "user.name", "h"]);
        run_git_in(work.path(), &["config", "commit.gpgSign", "false"]);

        // Two divergent commits, second one force-pushed over the first.
        run_git_in(work.path(), &["checkout", "-b", "divergent"]);
        std::fs::write(work.path().join("a.txt"), "1\n").unwrap();
        run_git_in(work.path(), &["add", "a.txt"]);
        run_git_in(work.path(), &["commit", "-m", "one", "--no-gpg-sign"]);
        run_git_in(work.path(), &["push", "origin", "divergent"]);

        run_git_in(work.path(), &["reset", "--hard", "HEAD~1"]);
        std::fs::write(work.path().join("b.txt"), "2\n").unwrap();
        run_git_in(work.path(), &["add", "b.txt"]);
        run_git_in(work.path(), &["commit", "-m", "two", "--no-gpg-sign"]);
        let force_out = std::process::Command::new("git")
            .args(["push", "--force", "origin", "divergent"])
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            force_out.status.success(),
            "force push rejected: {}",
            String::from_utf8_lossy(&force_out.stderr)
        );

        let delete_out = std::process::Command::new("git")
            .args(["push", "origin", "--delete", "divergent"])
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            delete_out.status.success(),
            "branch delete rejected: {}",
            String::from_utf8_lossy(&delete_out.stderr)
        );

        // Verify the branch is gone from the server side.
        let ls = std::process::Command::new("git")
            .args(["ls-remote", "--heads", server.url(), "divergent"])
            .output()
            .unwrap();
        assert!(ls.status.success());
        let stdout = String::from_utf8_lossy(&ls.stdout);
        assert!(
            stdout.trim().is_empty(),
            "divergent should be gone: {stdout}"
        );
    }

    fn run_git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_closes_active_keep_alive_connections() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let server = VanillaGitServer::start_empty("shutdown", reserve_port().unwrap())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", server.port))
                .await
                .unwrap();
            stream
                .write_all(b"GET /missing HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut status = String::new();
            reader.read_line(&mut status).await.unwrap();
            assert!(status.starts_with("HTTP/1.1 404"));
            // Receiving a response proves that the connection task exists;
            // shutdown must cancel it even though keep-alive remains open.
            server.stop().await;
            let mut remaining = Vec::new();
            reader.read_to_end(&mut remaining).await.unwrap();
        })
        .await
        .expect("shutdown must close accepted connections");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_io_drains_output_while_writing_input() {
        use std::process::Stdio;

        let directory = TempDir::new().unwrap();
        let payload = vec![b'x'; 1024 * 1024];
        let path = directory.path().join("payload");
        std::fs::write(&path, &payload).unwrap();
        let child = tokio::process::Command::new("sh")
            .args(["-c", "cat \"$1\"; cat \"$1\" >&2; cat", "test-io"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::collect_git_output(child, &payload),
        )
        .await
        .expect("all three pipes must make progress")
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, payload.repeat(2));
        assert_eq!(output.stderr, payload);
    }

    /// `start_empty` produces a server whose backing repo has zero refs
    /// — `ls-remote` succeeds with empty stdout. Liveness probe shape
    /// for the [`crate::Harness`] integration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_empty_ls_remote_returns_zero_refs() {
        let server = VanillaGitServer::start_empty("empty-test", reserve_port().unwrap())
            .await
            .unwrap();
        let out = tokio::process::Command::new("git")
            .args(["ls-remote", server.url()])
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "ls-remote on empty server failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.trim().is_empty(),
            "empty server should advertise zero refs; got: {stdout:?}"
        );
    }

    /// `start_empty` produces a writeable server — a test can push a
    /// branch into it from a side working tree, and the pushed tip is
    /// then visible via `ls-remote`. This is the canonical
    /// "harness-managed bare server, test seeds content on demand"
    /// pattern.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_empty_accepts_initial_push() {
        let server = VanillaGitServer::start_empty("empty-push", reserve_port().unwrap())
            .await
            .unwrap();

        let work = TempDir::new().unwrap();
        run_git(&work, &["init", "-b", "main"]);
        run_git(&work, &["config", "user.email", "h@e.com"]);
        run_git(&work, &["config", "user.name", "h"]);
        run_git(&work, &["config", "commit.gpgSign", "false"]);
        std::fs::write(work.path().join("seed.md"), "seed\n").unwrap();
        run_git(&work, &["add", "seed.md"]);
        run_git(&work, &["commit", "-m", "seed", "--no-gpg-sign"]);
        let head = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(work.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let push_out = std::process::Command::new("git")
            .args(["push", server.url(), "main:main"])
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            push_out.status.success(),
            "initial push to empty server failed: {}",
            String::from_utf8_lossy(&push_out.stderr)
        );

        let ls = std::process::Command::new("git")
            .args(["ls-remote", "--heads", server.url(), "main"])
            .output()
            .unwrap();
        assert!(ls.status.success());
        let stdout = String::from_utf8_lossy(&ls.stdout);
        assert!(
            stdout.contains(&head),
            "ls-remote missing pushed tip {head}; got: {stdout}"
        );
    }
}
