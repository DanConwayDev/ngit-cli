//! Deterministic in-process Blossom server fixture.
//!
//! ## Why raw TCP
//!
//! ngit's Blossom client is exercised at the socket level: several scenarios
//! need a connection which is accepted, read, and then closed without any
//! response at all, which a hyper service cannot express. The fixture
//! therefore speaks HTTP/1.1 directly over [`tokio::net::TcpStream`], reading
//! one request per connection and answering with `Connection: close`. Request
//! bodies must carry a `Content-Length`; ngit always sets one, even when it
//! streams a file.
//!
//! ## Model
//!
//! The default behaviour is an in-memory blob store which mirrors the parts of
//! BUD-01/BUD-02 that ngit uses:
//!
//! - `HEAD /<sha256>` answers `404` until the blob is stored and `200` with a
//!   matching `Content-Length` and `Content-Type` afterwards.
//! - `PUT /upload` stores the streamed body (validating `X-SHA-256` when the
//!   client sends one) and answers `201` with a valid blob descriptor.
//! - `PUT /mirror` stores the blob claimed by `X-SHA-256`, `X-Content-Length`,
//!   and `X-Content-Type` and answers with the same descriptor shape.
//!
//! [`BlossomRule`]s override that default for a request class, optionally
//! narrowed to one hash and/or a number of matches, so a test can script
//! definite HTTP failures, conflicting presence metadata, abrupt connection
//! closes, and stalls behind a [`BlossomGate`].
//!
//! ## No sleeps
//!
//! Every wait in this module is a [`tokio::sync`] primitive under a bounded
//! [`tokio::time::timeout`]. Tests observe progress through
//! [`BlossomServer::wait_for_requests`] /
//! [`BlossomServer::wait_for_request`] and release stalled or invisible blobs
//! through [`BlossomGate::open`], never through elapsed wall-clock time.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use bitcoin_hashes::sha256;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
};

/// Upper bound on a single captured request, head and body together.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Bound on one read from a connection which has already sent something.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on how long a stalled request waits for its gate.
const GATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on how long [`BlossomServer::finish`] waits for in-flight connections.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the request-observation helpers.
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(60);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking connection task must not turn every later assertion into a
    // poison error; the recorded failure list is the fixture's error channel.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The kind of Blossom operation a captured request represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlossomRequestKind {
    /// `HEAD /<sha256>` — a BUD-01 presence check.
    PresenceCheck,
    /// `PUT /upload` — a BUD-02 upload.
    Upload,
    /// `PUT /mirror` — a BUD-04 mirror.
    Mirror,
    /// Anything else the client sent.
    Other,
}

/// One request observed by a [`BlossomServer`].
#[derive(Clone, Debug)]
pub struct BlossomRequest {
    /// Zero-based index in the fixture's global request log.
    pub index: usize,
    pub method: String,
    /// Request target with any query string removed.
    pub path: String,
    /// The raw request head, terminated by its blank line.
    pub head: String,
    /// Header names and values in the order they were received.
    pub headers: Vec<(String, String)>,
    /// The request body. Empty when a rule closed the connection before the
    /// body was read.
    pub body: Vec<u8>,
    kind: BlossomRequestKind,
    hash: Option<String>,
}

impl BlossomRequest {
    /// Which Blossom operation this request represents.
    pub fn kind(&self) -> BlossomRequestKind {
        self.kind
    }

    /// The blob hash this request concerns: the path segment for a presence
    /// check, the `X-SHA-256` header for an upload or mirror.
    pub fn hash(&self) -> Option<&str> {
        self.hash.as_deref()
    }

    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(header, value)| header.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    /// `true` for `HEAD /<sha256>`.
    pub fn is_presence_check(&self) -> bool {
        self.kind == BlossomRequestKind::PresenceCheck
    }

    /// `true` for `PUT /upload`.
    pub fn is_upload(&self) -> bool {
        self.kind == BlossomRequestKind::Upload
    }

    /// `true` for `PUT /mirror`.
    pub fn is_mirror(&self) -> bool {
        self.kind == BlossomRequestKind::Mirror
    }
}

/// Every `PUT /upload` in a captured request log, in arrival order.
pub fn upload_requests(requests: &[BlossomRequest]) -> Vec<&BlossomRequest> {
    requests
        .iter()
        .filter(|request| request.is_upload())
        .collect()
}

/// Every `HEAD /<sha256>` in a captured request log, in arrival order.
pub fn presence_requests(requests: &[BlossomRequest]) -> Vec<&BlossomRequest> {
    requests
        .iter()
        .filter(|request| request.is_presence_check())
        .collect()
}

/// A test-controlled gate the fixture awaits under a bounded deadline.
///
/// Gates start closed and are opened exactly once by the test. They back both
/// [`BlossomRule::stall_until`] (hold the request, connection open, until the
/// test releases it) and [`BlossomServer::hide_uploads_until`] (accept an
/// upload but keep the blob invisible to presence checks).
#[derive(Clone, Debug)]
pub struct BlossomGate {
    sender: Arc<watch::Sender<bool>>,
}

impl Default for BlossomGate {
    fn default() -> Self {
        Self::closed()
    }
}

impl BlossomGate {
    /// A new, closed gate.
    pub fn closed() -> Self {
        Self {
            sender: Arc::new(watch::channel(false).0),
        }
    }

    /// Release everything waiting on this gate. Idempotent.
    ///
    /// `send_replace` rather than `send`: the gate's state must change even
    /// when nothing is currently waiting on it, which is exactly the case for
    /// [`BlossomServer::hide_uploads_until`].
    pub fn open(&self) {
        self.sender.send_replace(true);
    }

    /// Whether the gate has been opened.
    pub fn is_open(&self) -> bool {
        *self.sender.borrow()
    }

    async fn wait(&self) -> Result<()> {
        let mut receiver = self.sender.subscribe();
        tokio::time::timeout(GATE_TIMEOUT, receiver.wait_for(|open| *open))
            .await
            .context("Blossom fixture gate was not opened within its deadline")?
            .context("Blossom fixture gate was dropped before it opened")?;
        Ok(())
    }
}

/// Which requests a [`BlossomRule`] applies to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleTarget {
    Any,
    Kind(BlossomRequestKind),
}

#[derive(Clone, Debug)]
enum RuleResponse {
    /// Serve the request from the in-memory blob store.
    Store,
    /// Answer with a fixed status line and body.
    Status {
        code: u16,
        reason: String,
        content_type: String,
        body: String,
    },
    /// Answer a presence check `200` with metadata the client did not expect.
    PresenceMetadata { size: u64, content_type: String },
    /// Read the whole request, then close the socket without responding.
    CloseAfterRequest,
    /// Read only the request head, then close the socket without responding.
    CloseBeforeBody,
}

/// A scripted override for one class of Blossom request.
///
/// Rules are matched in registration order; the first rule whose target, hash,
/// and remaining-match count all admit the request wins. Requests matching no
/// rule are served from the in-memory blob store.
#[derive(Clone, Debug)]
pub struct BlossomRule {
    target: RuleTarget,
    sha256: Option<String>,
    remaining: Option<usize>,
    stall: Option<BlossomGate>,
    response: RuleResponse,
}

impl BlossomRule {
    fn with_target(target: RuleTarget) -> Self {
        Self {
            target,
            sha256: None,
            remaining: None,
            stall: None,
            response: RuleResponse::Store,
        }
    }

    /// Match every request.
    pub fn any() -> Self {
        Self::with_target(RuleTarget::Any)
    }

    /// Match `HEAD /<sha256>` presence checks.
    pub fn presence_check() -> Self {
        Self::with_target(RuleTarget::Kind(BlossomRequestKind::PresenceCheck))
    }

    /// Match `PUT /upload` requests.
    pub fn upload() -> Self {
        Self::with_target(RuleTarget::Kind(BlossomRequestKind::Upload))
    }

    /// Match `PUT /mirror` requests.
    pub fn mirror() -> Self {
        Self::with_target(RuleTarget::Kind(BlossomRequestKind::Mirror))
    }

    /// Narrow the rule to one blob hash.
    pub fn for_hash(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(sha256.into());
        self
    }

    /// Apply the rule to at most `count` requests. Unbounded by default.
    pub fn times(mut self, count: usize) -> Self {
        self.remaining = Some(count);
        self
    }

    /// Hold the request — connection open, nothing written — until `gate`
    /// opens, then apply the rest of the rule.
    pub fn stall_until(mut self, gate: &BlossomGate) -> Self {
        self.stall = Some(gate.clone());
        self
    }

    /// Answer with a fixed HTTP status and a plain-text body.
    pub fn respond_status(mut self, code: u16, reason: &str, body: &str) -> Self {
        self.response = RuleResponse::Status {
            code,
            reason: reason.to_owned(),
            content_type: "text/plain".to_owned(),
            body: body.to_owned(),
        };
        self
    }

    /// Answer a presence check `200 OK` with metadata which conflicts with the
    /// client's snapshot.
    pub fn respond_presence_metadata(mut self, size: u64, content_type: &str) -> Self {
        self.response = RuleResponse::PresenceMetadata {
            size,
            content_type: content_type.to_owned(),
        };
        self
    }

    /// Read the request, then close the connection without writing a response,
    /// leaving the outcome uncertain to the client.
    pub fn close_after_request(mut self) -> Self {
        self.response = RuleResponse::CloseAfterRequest;
        self
    }

    /// Close the connection once the request head has arrived, before the body
    /// is read. Identical to [`Self::close_after_request`] for bodyless
    /// requests such as presence checks.
    pub fn close_before_body(mut self) -> Self {
        self.response = RuleResponse::CloseBeforeBody;
        self
    }

    fn matches(&self, request: &BlossomRequest) -> bool {
        let target_matches = match self.target {
            RuleTarget::Any => true,
            RuleTarget::Kind(kind) => request.kind == kind,
        };
        let hash_matches = self
            .sha256
            .as_deref()
            .is_none_or(|sha256| request.hash() == Some(sha256));
        target_matches && hash_matches && self.remaining != Some(0)
    }
}

#[derive(Clone, Debug)]
struct StoredBlob {
    size: u64,
    content_type: String,
    /// When present, presence checks treat the blob as absent until the gate
    /// opens.
    visibility: Option<BlossomGate>,
}

impl StoredBlob {
    fn is_visible(&self) -> bool {
        self.visibility.as_ref().is_none_or(BlossomGate::is_open)
    }
}

struct BlossomState {
    base_url: String,
    blobs: Mutex<BTreeMap<String, StoredBlob>>,
    rules: Mutex<Vec<BlossomRule>>,
    log: Mutex<Vec<BlossomRequest>>,
    observed: watch::Sender<usize>,
    failures: Mutex<Vec<String>>,
    upload_visibility: Mutex<Option<BlossomGate>>,
    connections: Mutex<Vec<JoinHandle<()>>>,
}

impl BlossomState {
    fn record_failure(&self, error: &anyhow::Error) {
        lock(&self.failures).push(format!("{error:#}"));
    }

    fn record_request(&self, request: &mut BlossomRequest) {
        let mut log = lock(&self.log);
        request.index = log.len();
        log.push(request.clone());
        let observed = log.len();
        drop(log);
        self.observed.send_replace(observed);
    }

    /// Resolve the rule governing `request`, consuming one of its remaining
    /// matches.
    fn take_rule(&self, request: &BlossomRequest) -> Option<BlossomRule> {
        let mut rules = lock(&self.rules);
        let rule = rules.iter_mut().find(|rule| rule.matches(request))?;
        if let Some(remaining) = rule.remaining.as_mut() {
            *remaining = remaining.saturating_sub(1);
        }
        Some(rule.clone())
    }

    fn store_blob(&self, sha256: &str, size: u64, content_type: &str) {
        let visibility = lock(&self.upload_visibility).clone();
        lock(&self.blobs).insert(
            sha256.to_owned(),
            StoredBlob {
                size,
                content_type: content_type.to_owned(),
                visibility,
            },
        );
    }

    fn visible_blob(&self, sha256: &str) -> Option<StoredBlob> {
        lock(&self.blobs)
            .get(sha256)
            .filter(|blob| blob.is_visible())
            .cloned()
    }

    fn descriptor(&self, sha256: &str, size: u64, content_type: &str) -> String {
        serde_json::json!({
            "url": blob_url(&self.base_url, sha256),
            "sha256": sha256,
            "size": size,
            "type": content_type,
            "uploaded": 1,
        })
        .to_string()
    }
}

fn blob_url(base_url: &str, sha256: &str) -> String {
    format!("{}/{sha256}", base_url.trim_end_matches('/'))
}

/// A loopback Blossom server which records every request it serves.
///
/// The fixture accepts connections until [`BlossomServer::finish`] is called
/// or the value is dropped; there is no request budget. Assert on the captured
/// log rather than on a served-request count.
pub struct BlossomServer {
    base_url: String,
    state: Arc<BlossomState>,
    accept_task: Option<JoinHandle<()>>,
}

impl BlossomServer {
    /// Bind an ephemeral loopback port and start serving from an empty blob
    /// store.
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind the Blossom fixture")?;
        let address = listener
            .local_addr()
            .context("failed to read the Blossom fixture address")?;
        let base_url = format!("http://{address}");
        let state = Arc::new(BlossomState {
            base_url: base_url.clone(),
            blobs: Mutex::new(BTreeMap::new()),
            rules: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            observed: watch::channel(0).0,
            failures: Mutex::new(Vec::new()),
            upload_visibility: Mutex::new(None),
            connections: Mutex::new(Vec::new()),
        });
        let accept_state = Arc::clone(&state);
        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&accept_state);
                let handle = tokio::spawn(async move {
                    if let Err(error) = serve_connection(&state, stream).await {
                        state.record_failure(&error);
                    }
                });
                lock(&accept_state.connections).push(handle);
            }
        });
        Ok(Self {
            base_url,
            state,
            accept_task: Some(accept_task),
        })
    }

    /// The fixture's origin, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The fixture's origin with a trailing slash, matching how ngit
    /// normalises a Blossom server URL in its output.
    pub fn base_url_with_slash(&self) -> String {
        format!("{}/", self.base_url)
    }

    /// The descriptor URL the fixture reports for `sha256`.
    pub fn blob_url(&self, sha256: &str) -> String {
        blob_url(&self.base_url, sha256)
    }

    /// Register a scripted override. Rules are matched in registration order.
    pub fn add_rule(&self, rule: BlossomRule) {
        lock(&self.state.rules).push(rule);
    }

    /// Pre-seed `bytes` so presence checks answer `200` before any upload,
    /// returning the blob's hash.
    pub fn seed_blob(&self, bytes: &[u8], content_type: &str) -> String {
        let sha256 = sha256::Hash::hash(bytes).to_string();
        self.seed_blob_metadata(&sha256, bytes.len() as u64, content_type);
        sha256
    }

    /// Pre-seed a blob by hash and metadata alone, for bytes the test does not
    /// hold.
    pub fn seed_blob_metadata(&self, sha256: &str, size: u64, content_type: &str) {
        lock(&self.state.blobs).insert(
            sha256.to_owned(),
            StoredBlob {
                size,
                content_type: content_type.to_owned(),
                visibility: None,
            },
        );
    }

    /// Accept uploads normally but keep the stored blobs invisible to presence
    /// checks until `gate` opens — the "accepted but not yet visible" server.
    ///
    /// Blobs stored before this call keep their existing visibility.
    pub fn hide_uploads_until(&self, gate: &BlossomGate) {
        *lock(&self.state.upload_visibility) = Some(gate.clone());
    }

    /// Snapshot the request log without ending the fixture.
    pub fn requests(&self) -> Vec<BlossomRequest> {
        lock(&self.state.log).clone()
    }

    /// Number of requests captured so far.
    pub fn request_count(&self) -> usize {
        lock(&self.state.log).len()
    }

    /// Wait, under a bounded deadline, until at least `count` requests have
    /// been captured, then return the log snapshot.
    pub async fn wait_for_requests(&self, count: usize) -> Result<Vec<BlossomRequest>> {
        self.wait_until(|requests| requests.len() >= count)
            .await
            .with_context(|| format!("Blossom fixture did not observe {count} requests"))
    }

    /// Wait, under a bounded deadline, until a captured request satisfies
    /// `predicate`, then return that request.
    ///
    /// `description` names the awaited request in the timeout error.
    pub async fn wait_for_request(
        &self,
        description: &str,
        predicate: impl Fn(&BlossomRequest) -> bool,
    ) -> Result<BlossomRequest> {
        let requests = self
            .wait_until(|requests| requests.iter().any(&predicate))
            .await
            .with_context(|| format!("Blossom fixture did not observe {description}"))?;
        requests
            .into_iter()
            .find(|request| predicate(request))
            .context("matched Blossom request disappeared from the log")
    }

    async fn wait_until(
        &self,
        satisfied: impl Fn(&[BlossomRequest]) -> bool,
    ) -> Result<Vec<BlossomRequest>> {
        let mut observed = self.state.observed.subscribe();
        tokio::time::timeout(OBSERVATION_TIMEOUT, async {
            loop {
                let requests = self.requests();
                if satisfied(&requests) {
                    return Ok(requests);
                }
                observed.changed().await.context(
                    "Blossom fixture stopped recording requests before the condition was met",
                )?;
            }
        })
        .await
        .context("timed out")?
    }

    /// Stop accepting connections, join the in-flight ones under a bounded
    /// deadline, and return the captured request log.
    ///
    /// Fails if a connection handler reported a protocol error, for example an
    /// upload whose body did not match its `X-SHA-256` header.
    pub async fn finish(mut self) -> Result<Vec<BlossomRequest>> {
        let accept_task = self
            .accept_task
            .take()
            .context("Blossom fixture accept task missing")?;
        accept_task.abort();
        let connections = std::mem::take(&mut *lock(&self.state.connections));
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        for mut connection in connections {
            if tokio::time::timeout_at(deadline, &mut connection)
                .await
                .is_err()
            {
                // A deliberately stalled or dropped connection may still be
                // open; it has already been captured in the request log.
                connection.abort();
            }
        }
        let failures = std::mem::take(&mut *lock(&self.state.failures));
        ensure!(
            failures.is_empty(),
            "Blossom fixture reported {} connection failure(s): {}",
            failures.len(),
            failures.join("; ")
        );
        Ok(self.requests())
    }
}

impl Drop for BlossomServer {
    fn drop(&mut self) {
        if let Some(task) = &self.accept_task {
            task.abort();
        }
        for connection in lock(&self.state.connections).iter() {
            connection.abort();
        }
    }
}

async fn serve_connection(state: &Arc<BlossomState>, mut stream: TcpStream) -> Result<()> {
    let Some((head, body_prefix)) = read_request_head(&mut stream).await? else {
        // A connection the client opened and closed without sending anything
        // is not a fixture failure.
        return Ok(());
    };
    let mut request = parse_request_head(&head)?;
    let content_length = request
        .header("content-length")
        .map(str::parse::<usize>)
        .transpose()
        .context("Blossom request used an invalid Content-Length")?
        .unwrap_or(0);
    ensure!(
        request.header("transfer-encoding").is_none(),
        "the Blossom fixture requires a Content-Length request body"
    );

    let rule = {
        // Match the rule against the head so `close_before_body` can fire
        // before the body is read; the recorded rule is reused afterwards.
        let mut probe = request.clone();
        probe.body = Vec::new();
        state.take_rule(&probe)
    };
    let close_before_body = matches!(
        rule.as_ref().map(|rule| &rule.response),
        Some(RuleResponse::CloseBeforeBody)
    );
    if !close_before_body {
        request.body = read_request_body(&mut stream, body_prefix, content_length).await?;
    }
    state.record_request(&mut request);
    if close_before_body {
        return Ok(());
    }

    if let Some(gate) = rule.as_ref().and_then(|rule| rule.stall.as_ref()) {
        gate.wait().await?;
    }
    let response = match rule.as_ref().map(|rule| &rule.response) {
        None | Some(RuleResponse::Store) => store_response(state, &request)?,
        Some(RuleResponse::Status {
            code,
            reason,
            content_type,
            body,
        }) => http_response(*code, reason, Some((content_type, body.as_str()))),
        Some(RuleResponse::PresenceMetadata { size, content_type }) => {
            presence_response(*size, content_type)
        }
        Some(RuleResponse::CloseAfterRequest) => return Ok(()),
        Some(RuleResponse::CloseBeforeBody) => unreachable!("handled before the body was read"),
    };
    // A response to HEAD carries the entity headers but never the entity.
    let response = if request.method.eq_ignore_ascii_case("HEAD") {
        response
            .find("\r\n\r\n")
            .map_or(response.clone(), |end| response[..end + 4].to_owned())
    } else {
        response
    };
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write the Blossom fixture response")?;
    stream
        .shutdown()
        .await
        .context("failed to finish the Blossom fixture response")?;
    Ok(())
}

fn store_response(state: &Arc<BlossomState>, request: &BlossomRequest) -> Result<String> {
    match request.kind {
        BlossomRequestKind::PresenceCheck => {
            let sha256 = request.hash().context("presence check omitted a hash")?;
            Ok(state.visible_blob(sha256).map_or_else(
                || http_response(404, "Not Found", None),
                |blob| presence_response(blob.size, &blob.content_type),
            ))
        }
        BlossomRequestKind::Upload => {
            let content_type = request
                .header("content-type")
                .unwrap_or("application/octet-stream")
                .to_owned();
            let sha256 = sha256::Hash::hash(&request.body).to_string();
            if let Some(claimed) = request.hash() {
                ensure!(
                    claimed == sha256,
                    "Blossom upload claimed X-SHA-256 {claimed} but sent {sha256}"
                );
            }
            let size = request.body.len() as u64;
            state.store_blob(&sha256, size, &content_type);
            Ok(http_response(
                201,
                "Created",
                Some((
                    "application/json",
                    state.descriptor(&sha256, size, &content_type).as_str(),
                )),
            ))
        }
        BlossomRequestKind::Mirror => {
            let sha256 = request
                .hash()
                .context("Blossom mirror omitted X-SHA-256")?
                .to_owned();
            let size = request
                .header("x-content-length")
                .context("Blossom mirror omitted X-Content-Length")?
                .parse::<u64>()
                .context("Blossom mirror sent an invalid X-Content-Length")?;
            let content_type = request
                .header("x-content-type")
                .unwrap_or("application/octet-stream")
                .to_owned();
            state.store_blob(&sha256, size, &content_type);
            Ok(http_response(
                201,
                "Created",
                Some((
                    "application/json",
                    state.descriptor(&sha256, size, &content_type).as_str(),
                )),
            ))
        }
        BlossomRequestKind::Other => bail!(
            "the Blossom fixture received an unexpected request: {} {}",
            request.method,
            request.path
        ),
    }
}

fn presence_response(size: u64, content_type: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
    )
}

fn http_response(code: u16, reason: &str, body: Option<(&str, &str)>) -> String {
    let (content_type, body) = body.unwrap_or(("text/plain", ""));
    format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Read up to and including the blank line ending the request head, returning
/// it with any body bytes which arrived in the same read. `None` means the
/// client closed the connection without sending anything.
async fn read_request_head(stream: &mut TcpStream) -> Result<Option<(String, Vec<u8>)>> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
        ensure!(
            bytes.len() < MAX_REQUEST_BYTES,
            "Blossom request headers exceeded the fixture limit"
        );
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .context("timed out reading Blossom request headers")?
            .context("failed to read Blossom request headers")?;
        if read == 0 {
            ensure!(
                bytes.is_empty(),
                "Blossom client closed part way through its request headers"
            );
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let body_prefix = bytes[header_end..].to_vec();
    bytes.truncate(header_end);
    let head = String::from_utf8(bytes).context("Blossom request headers were not UTF-8")?;
    Ok(Some((head, body_prefix)))
}

async fn read_request_body(
    stream: &mut TcpStream,
    prefix: Vec<u8>,
    content_length: usize,
) -> Result<Vec<u8>> {
    ensure!(
        content_length <= MAX_REQUEST_BYTES,
        "Blossom request body exceeded the fixture limit"
    );
    ensure!(
        prefix.len() <= content_length,
        "Blossom client sent more body bytes than its Content-Length"
    );
    let mut body = prefix;
    body.reserve(content_length.saturating_sub(body.len()).min(64 * 1024));
    while body.len() < content_length {
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .context("timed out reading a Blossom request body")?
            .context("failed to read a Blossom request body")?;
        ensure!(read != 0, "Blossom client closed before sending its body");
        let wanted = content_length - body.len();
        body.extend_from_slice(&chunk[..read.min(wanted)]);
    }
    Ok(body)
}

fn parse_request_head(head: &str) -> Result<BlossomRequest> {
    let mut lines = head.lines();
    let request_line = lines
        .next()
        .context("Blossom request omitted its request line")?;
    let mut fields = request_line.split_whitespace();
    let method = fields
        .next()
        .context("Blossom request omitted its method")?
        .to_owned();
    let target = fields
        .next()
        .context("Blossom request omitted its target")?;
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let headers = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect::<Vec<_>>();

    let mut request = BlossomRequest {
        index: 0,
        method,
        path,
        head: head.to_owned(),
        headers,
        body: Vec::new(),
        kind: BlossomRequestKind::Other,
        hash: None,
    };
    request.kind = match (request.method.as_str(), request.path.as_str()) {
        ("PUT", "/upload") => BlossomRequestKind::Upload,
        ("PUT", "/mirror") => BlossomRequestKind::Mirror,
        ("HEAD", _) => BlossomRequestKind::PresenceCheck,
        _ => BlossomRequestKind::Other,
    };
    request.hash = match request.kind {
        BlossomRequestKind::PresenceCheck => request
            .path
            .trim_start_matches('/')
            .split('.')
            .next()
            .filter(|hash| !hash.is_empty())
            .map(str::to_owned),
        BlossomRequestKind::Upload | BlossomRequestKind::Mirror => {
            request.header("x-sha-256").map(str::to_owned)
        }
        BlossomRequestKind::Other => None,
    };
    Ok(request)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::{StatusCode, redirect::Policy};

    use super::*;

    const BLOB: &[u8] = b"deterministic blossom fixture blob\n";

    fn client() -> reqwest::Client {
        // Reqwest initializes TLS even for loopback HTTP. Match ngit's Ring
        // selection, preserving any provider installed by an earlier caller.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        // Mirrors the parts of ngit's Blossom client which decide what a
        // failure looks like: no redirect following, bounded idle reads.
        reqwest::Client::builder()
            .redirect(Policy::none())
            .read_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build the fixture test client")
    }

    async fn upload(client: &reqwest::Client, server: &BlossomServer, bytes: &[u8]) -> Result<()> {
        let response = client
            .put(format!("{}/upload", server.base_url()))
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", sha256::Hash::hash(bytes).to_string())
            .body(bytes.to_vec())
            .send()
            .await
            .context("upload request failed")?;
        ensure!(response.status() == StatusCode::CREATED, "upload rejected");
        let descriptor: serde_json::Value =
            response.json().await.context("descriptor was not JSON")?;
        ensure!(descriptor["sha256"] == sha256::Hash::hash(bytes).to_string());
        ensure!(descriptor["size"] == bytes.len());
        Ok(())
    }

    async fn head(client: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
        client.head(url).send().await.context("HEAD request failed")
    }

    #[tokio::test]
    async fn default_store_answers_presence_checks_around_an_upload() -> Result<()> {
        let server = BlossomServer::start().await?;
        let client = client();
        let hash = sha256::Hash::hash(BLOB).to_string();
        let url = server.blob_url(&hash);

        assert_eq!(head(&client, &url).await?.status(), StatusCode::NOT_FOUND);
        upload(&client, &server, BLOB).await?;
        let present = head(&client, &url).await?;
        assert_eq!(present.status(), StatusCode::OK);
        assert_eq!(
            present.headers().get("content-length").unwrap(),
            BLOB.len().to_string().as_str()
        );
        assert_eq!(
            present.headers().get("content-type").unwrap(),
            "application/octet-stream"
        );

        let requests = server.finish().await?;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].is_presence_check());
        assert_eq!(requests[0].hash(), Some(hash.as_str()));
        assert!(requests[1].is_upload());
        assert_eq!(requests[1].body, BLOB);
        assert_eq!(requests[1].header("x-sha-256"), Some(hash.as_str()));
        assert!(requests[2].is_presence_check());
        assert_eq!(
            requests
                .iter()
                .map(|request| request.index)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        Ok(())
    }

    #[tokio::test]
    async fn seeded_blobs_are_present_before_any_upload() -> Result<()> {
        let server = BlossomServer::start().await?;
        let hash = server.seed_blob(BLOB, "application/zip");
        let response = head(&client(), &server.blob_url(&hash)).await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/zip"
        );
        assert_eq!(upload_requests(&server.finish().await?).len(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn scripted_status_and_metadata_rules_apply_in_order() -> Result<()> {
        let server = BlossomServer::start().await?;
        let hash = server.seed_blob(BLOB, "application/zip");
        server.add_rule(
            BlossomRule::presence_check()
                .for_hash(&hash)
                .times(1)
                .respond_status(500, "Internal Server Error", "presence check failed"),
        );
        server.add_rule(
            BlossomRule::presence_check()
                .for_hash(&hash)
                .times(1)
                .respond_presence_metadata(1, "text/plain"),
        );
        let client = client();
        let url = server.blob_url(&hash);

        assert_eq!(
            head(&client, &url).await?.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let conflicting = head(&client, &url).await?;
        assert_eq!(conflicting.status(), StatusCode::OK);
        assert_eq!(conflicting.headers().get("content-length").unwrap(), "1");
        assert_eq!(
            conflicting.headers().get("content-type").unwrap(),
            "text/plain"
        );
        // The scripted rules are exhausted; the store answers again.
        assert_eq!(head(&client, &url).await?.status(), StatusCode::OK);

        assert_eq!(presence_requests(&server.finish().await?).len(), 3);
        Ok(())
    }

    /// Documents exactly what a `reqwest` client observes when the fixture
    /// accepts a request and then closes the socket without responding: a
    /// `reqwest::Error` of kind `Request`, wrapping hyper's
    /// `IncompleteMessage` ("connection closed before message completed"). It
    /// is neither a connect nor a builder error, so ngit's
    /// `classify_send_error` treats it as a retryable transport failure whose
    /// storage outcome is uncertain, rather than as a definite miss.
    #[tokio::test]
    async fn closing_a_connection_leaves_the_client_without_a_response() -> Result<()> {
        let server = BlossomServer::start().await?;
        let hash = sha256::Hash::hash(BLOB).to_string();
        server.add_rule(BlossomRule::presence_check().times(1).close_after_request());
        server.add_rule(BlossomRule::upload().times(1).close_before_body());

        let client = client();
        let error = client
            .head(server.blob_url(&hash))
            .send()
            .await
            .expect_err("an unanswered request must fail");
        assert!(error.is_request(), "unexpected error kind: {error:?}");
        assert!(!error.is_connect(), "the connection was accepted: {error}");
        assert!(!error.is_builder());
        assert!(!error.is_timeout(), "the socket closed promptly: {error}");
        let mut source = std::error::Error::source(&error);
        let mut chain = Vec::new();
        while let Some(error) = source {
            chain.push(error.to_string());
            source = error.source();
        }
        assert!(
            chain
                .iter()
                .any(|error| error == "connection closed before message completed"),
            "unexpected error chain: {chain:?}"
        );

        let upload = client
            .put(format!("{}/upload", server.base_url()))
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", &hash)
            .body(BLOB.to_vec())
            .send()
            .await;
        assert!(upload.is_err(), "a dropped upload must fail");

        let requests = server.finish().await?;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].is_presence_check());
        assert!(requests[1].is_upload());
        assert!(
            requests[1].body.is_empty(),
            "close_before_body must not read the body"
        );
        Ok(())
    }

    #[tokio::test]
    async fn uploads_stay_invisible_until_the_visibility_gate_opens() -> Result<()> {
        let server = BlossomServer::start().await?;
        let gate = BlossomGate::closed();
        server.hide_uploads_until(&gate);
        let client = client();
        let hash = sha256::Hash::hash(BLOB).to_string();
        let url = server.blob_url(&hash);

        upload(&client, &server, BLOB).await?;
        assert_eq!(head(&client, &url).await?.status(), StatusCode::NOT_FOUND);
        gate.open();
        assert_eq!(head(&client, &url).await?.status(), StatusCode::OK);

        assert_eq!(server.finish().await?.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn a_stalled_request_is_held_until_its_gate_opens() -> Result<()> {
        let server = BlossomServer::start().await?;
        let gate = BlossomGate::closed();
        let hash = server.seed_blob(BLOB, "application/zip");
        server.add_rule(BlossomRule::presence_check().times(1).stall_until(&gate));
        let client = client();
        let url = server.blob_url(&hash);

        let stalled = tokio::spawn({
            let client = client.clone();
            let url = url.clone();
            async move { client.head(url).send().await }
        });
        // The stalled request is observable while it is still unanswered, and
        // an unrelated connection is served concurrently.
        let held = server
            .wait_for_request(
                "the stalled presence check",
                BlossomRequest::is_presence_check,
            )
            .await?;
        assert_eq!(held.hash(), Some(hash.as_str()));
        assert!(!stalled.is_finished(), "the gate must hold the response");
        assert_eq!(head(&client, &url).await?.status(), StatusCode::OK);

        gate.open();
        let response = tokio::time::timeout(Duration::from_secs(10), stalled)
            .await
            .context("the stalled request was not released by its gate")???;
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(server.wait_for_requests(2).await?.len(), 2);
        assert_eq!(server.finish().await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn mirror_requests_store_the_claimed_blob() -> Result<()> {
        let server = BlossomServer::start().await?;
        let client = client();
        let hash = sha256::Hash::hash(BLOB).to_string();

        let response = client
            .put(format!("{}/mirror", server.base_url()))
            .header("X-SHA-256", &hash)
            .header("X-Content-Length", BLOB.len())
            .header("X-Content-Type", "application/zip")
            .json(&serde_json::json!({ "url": server.blob_url(&hash) }))
            .send()
            .await
            .context("mirror request failed")?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let descriptor: serde_json::Value = response.json().await?;
        assert_eq!(descriptor["url"], server.blob_url(&hash));
        assert_eq!(
            head(&client, &server.blob_url(&hash)).await?.status(),
            StatusCode::OK
        );

        let requests = server.finish().await?;
        assert!(requests[0].is_mirror());
        assert_eq!(requests[0].hash(), Some(hash.as_str()));
        Ok(())
    }

    #[tokio::test]
    async fn an_upload_that_lies_about_its_hash_is_a_fixture_failure() -> Result<()> {
        let server = BlossomServer::start().await?;
        let response = client()
            .put(format!("{}/upload", server.base_url()))
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", "0".repeat(64))
            .body(BLOB.to_vec())
            .send()
            .await;
        assert!(
            response.is_err(),
            "the fixture must not answer a bad upload"
        );

        let error = server
            .finish()
            .await
            .expect_err("the fixture must report the mismatch");
        assert!(format!("{error:#}").contains("claimed X-SHA-256"));
        Ok(())
    }
}
