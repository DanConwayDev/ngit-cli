//! Blossom transport support shared by releases, containers, and nsites.
//!
//! Local files are copied into a private temporary file before an upload is
//! attempted. The snapshot makes the hash, size, and bytes sent to a Blossom
//! server one immutable unit even if the source path changes later.

use std::{
    collections::{BTreeSet, HashSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use bitcoin_hashes::{HashEngine as _, sha256};
use futures::{StreamExt as _, stream};
use nostr::prelude::{Event, EventBuilder, EventId, Filter, Kind, PublicKey, Tag, Timestamp};
use reqwest::{
    StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LOCATION},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio_util::io::ReaderStream;

use crate::{
    event_ordering::latest_event,
    release_download::{
        DEFAULT_MAX_ASSET_BYTES, DownloadWarning, DownloadWarningCode, infer_mime_type,
        sanitize_filename,
    },
    signer::NgitSigner,
};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const AUTHORIZATION_LIFETIME: Duration = Duration::from_secs(5 * 60);
const AUTHORIZATION_SIGNING_ALLOWANCE: Duration = Duration::from_secs(2 * 60 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PRESENCE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const PRESENCE_CIRCUIT_FAILURE_THRESHOLD: usize = 3;
const UPLOAD_REQUEST_TIMEOUT: Duration = TOTAL_TIMEOUT;
const PLACEMENT_MAX_ATTEMPTS: usize = 3;
const PLACEMENT_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;
const MAX_PRESENCE_REDIRECTS: usize = 5;
pub const DEFAULT_AUTHORIZATION_BATCH_SIZE: usize = 20;
pub const DEFAULT_UPLOAD_CONCURRENCY: usize = 4;

#[derive(Clone, Copy, Debug)]
enum AuthorizationEncodingPreference {
    Bud11,
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlacementRequirement {
    EveryServer,
    OneServerPerBlob,
}

#[derive(Default)]
struct PresenceCircuit {
    consecutive_transient_failures: AtomicUsize,
    open: AtomicBool,
}

impl PresenceCircuit {
    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    fn record_success(&self) {
        if !self.is_open() {
            self.consecutive_transient_failures
                .store(0, Ordering::Release);
        }
    }

    fn record_transient_failure(&self) -> bool {
        let failures = self
            .consecutive_transient_failures
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if failures >= PRESENCE_CIRCUIT_FAILURE_THRESHOLD {
            self.open.store(true, Ordering::Release);
        }
        self.is_open()
    }
}

enum PresenceProbeResult {
    Checked(std::result::Result<bool, BlobRequestError>),
    Skipped,
}

#[derive(Clone, Copy)]
struct BatchAuthorizationOptions {
    batch_size: usize,
    encoding_preference: AuthorizationEncodingPreference,
    placement_requirement: PlacementRequirement,
}

impl BatchAuthorizationOptions {
    const DEFAULT: Self = Self {
        batch_size: DEFAULT_AUTHORIZATION_BATCH_SIZE,
        encoding_preference: AuthorizationEncodingPreference::Bud11,
        placement_requirement: PlacementRequirement::EveryServer,
    };

    // Release assets must interoperate with deployed servers which accept only
    // one `x` tag and padded standard Base64. The same signed per-blob event is
    // still reused across every selected server.
    const LEGACY_PER_BLOB: Self = Self {
        batch_size: 1,
        encoding_preference: AuthorizationEncodingPreference::Legacy,
        placement_requirement: PlacementRequirement::OneServerPerBlob,
    };

    const RESILIENT: Self = Self {
        batch_size: DEFAULT_AUTHORIZATION_BATCH_SIZE,
        encoding_preference: AuthorizationEncodingPreference::Bud11,
        placement_requirement: PlacementRequirement::OneServerPerBlob,
    };
}

/// Observable milestones emitted by the Blossom batch placement engine.
///
/// Byte totals describe HTTP request bodies, so a retry or compatible
/// authorization fallback extends the total before its replacement PUT starts.
/// This keeps aggregate progress monotonic even when several placements run
/// concurrently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlossomProgressEvent {
    PresenceChecksStarted {
        blobs: usize,
        servers: usize,
        checks: usize,
    },
    PresenceCheckFinished {
        checked: usize,
        checks: usize,
        confirmed: usize,
        missing: usize,
        unavailable: usize,
    },
    PresenceChecksFinished {
        missing: usize,
    },
    AuthorizationStarted {
        batch: usize,
        batches: usize,
        blobs: usize,
        filenames: Vec<String>,
    },
    UploadBatchStarted {
        batch: usize,
        batches: usize,
        blobs: usize,
        filenames: Vec<String>,
        placements: usize,
        confirmed: usize,
        unavailable: usize,
        bytes: u64,
    },
    UploadRequestStarted {
        batch: usize,
        batches: usize,
        filename: String,
        server: Url,
        attempt: usize,
        max_attempts: usize,
        total_bytes: u64,
        additional_bytes: u64,
    },
    UploadedBytes {
        batch: usize,
        filename: String,
        server: Url,
        bytes: u64,
    },
    UploadBodyFinished {
        batch: usize,
        batches: usize,
        filename: String,
        server: Url,
        attempt: usize,
        max_attempts: usize,
        idle_timeout_secs: u64,
    },
    VerificationStarted {
        batch: usize,
        batches: usize,
        filename: String,
        server: Url,
        attempt: usize,
        max_attempts: usize,
        timeout_secs: u64,
    },
    RetryScheduled {
        batch: usize,
        batches: usize,
        filename: String,
        server: Url,
        next_attempt: usize,
        max_attempts: usize,
    },
    PlacementFinished {
        filename: String,
        server: Url,
        status: BlossomServerStatus,
        message: Option<String>,
    },
    UploadBatchFinished {
        batch: usize,
        batches: usize,
    },
}

/// Receives synchronous progress updates from a Blossom batch placement.
pub trait BlossomProgress: Send + Sync {
    fn update(&self, event: &BlossomProgressEvent);
}

struct HiddenBlossomProgress;

impl BlossomProgress for HiddenBlossomProgress {
    fn update(&self, _event: &BlossomProgressEvent) {}
}

/// A local file which should be staged for a Blossom upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFileRequest {
    pub source_path: PathBuf,
    /// Caller-supplied published filename, taking precedence over the basename.
    pub filename: Option<String>,
    /// Caller-supplied MIME type. The filename extension is used when absent.
    pub mime_type: Option<String>,
    /// Maximum number of bytes copied into the stable snapshot.
    pub max_bytes: u64,
}

impl LocalFileRequest {
    pub fn new(source_path: impl Into<PathBuf>) -> Self {
        Self {
            source_path: source_path.into(),
            filename: None,
            mime_type: None,
            max_bytes: DEFAULT_MAX_ASSET_BYTES,
        }
    }
}

/// Immutable bytes and metadata prepared for one or more Blossom uploads.
#[derive(Debug)]
pub struct FileSnapshot {
    file: NamedTempFile,
    pub filename: String,
    pub mime_type: String,
    /// Lowercase, 64-character SHA-256 of the snapshotted bytes.
    pub sha256: String,
    pub size: u64,
    pub warnings: Vec<DownloadWarning>,
}

impl FileSnapshot {
    pub(crate) fn reopen(&self) -> Result<File> {
        self.file
            .reopen()
            .context("failed to reopen the stable asset snapshot")
    }

    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }
}

/// A server-confirmed BUD-02 blob descriptor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobDescriptor {
    pub url: Url,
    pub sha256: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub uploaded: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BlobUpload {
    descriptor: BlobDescriptor,
    /// `true` for `201 Created`; `false` for `200 OK` (already present).
    newly_stored: bool,
}

/// Operation assigned to one server in a multi-server upload plan.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlossomServerOperation {
    Upload,
    Mirror,
}

/// Observable result for one planned Blossom server operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlossomServerStatus {
    /// The server returned `201 Created` with a valid descriptor.
    Stored,
    /// The server returned `200 OK` with a valid descriptor.
    AlreadyPresent,
    /// The server definitely rejected the request or its descriptor.
    Failed,
    /// The transport ended without proving whether the server stored the blob.
    Unknown,
    /// An earlier operation failed, so this server was not contacted.
    NotAttempted,
}

/// Stable per-server output from a multi-server upload plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlossomServerOutcome {
    pub server: Url,
    pub operation: BlossomServerOperation,
    pub status: BlossomServerStatus,
    pub descriptor: Option<BlobDescriptor>,
    pub message: Option<String>,
}

/// A blob which a failed workflow may have stored without publishing its event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PossibleOrphanBlob {
    pub server: Url,
    pub sha256: String,
    pub url: Option<Url>,
}

/// Successful result of confirming one blob on one or more selected servers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MultiServerUpload {
    /// A descriptor for the first confirmed server, whose URL is suitable for
    /// a NIP-82 `url` tag. When a strict HEAD proves the blob was already
    /// present, ngit synthesizes this descriptor from the verified metadata
    /// and uses `uploaded: 0` because HEAD does not expose the original upload
    /// time.
    pub primary: BlobDescriptor,
    pub servers: Vec<BlossomServerOutcome>,
}

/// Complete failure record for an ordered multi-server upload plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MultiServerUploadError {
    pub message: String,
    pub servers: Vec<BlossomServerOutcome>,
    pub possible_orphan_blobs: Vec<PossibleOrphanBlob>,
}

/// Per-blob output from a batch which attempts every selected server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchBlobUploadOutcome {
    pub sha256: String,
    pub servers: Vec<BlossomServerOutcome>,
}

/// Successful result satisfying the caller's placement requirement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchUploadResult {
    pub blobs: Vec<BatchBlobUploadOutcome>,
}

/// Complete failure record for a batch upload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchUploadError {
    pub message: String,
    pub blobs: Vec<BatchBlobUploadOutcome>,
    pub possible_orphan_blobs: Vec<PossibleOrphanBlob>,
}

impl std::fmt::Display for BatchUploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BatchUploadError {}

impl std::fmt::Display for MultiServerUploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MultiServerUploadError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestFailureKind {
    Definite,
    Unknown,
}

#[derive(Debug)]
struct BlobRequestError {
    kind: RequestFailureKind,
    message: String,
    possible_orphan: bool,
    status: Option<StatusCode>,
    retryable: bool,
}

impl BlobRequestError {
    fn definite(error: anyhow::Error, possible_orphan: bool) -> Self {
        Self {
            kind: RequestFailureKind::Definite,
            message: format!("{error:#}"),
            possible_orphan,
            status: None,
            retryable: false,
        }
    }

    fn unknown(error: anyhow::Error, possible_orphan: bool) -> Self {
        Self {
            kind: RequestFailureKind::Unknown,
            message: format!("{error:#}"),
            possible_orphan,
            status: None,
            retryable: true,
        }
    }

    fn http(
        error: anyhow::Error,
        status: StatusCode,
        kind: RequestFailureKind,
        possible_orphan: bool,
    ) -> Self {
        Self {
            kind,
            message: format!("{error:#}"),
            possible_orphan,
            status: Some(status),
            retryable: is_transient_status(status),
        }
    }

    fn transport(error: anyhow::Error, possible_orphan: bool) -> Self {
        Self {
            kind: if possible_orphan {
                RequestFailureKind::Unknown
            } else {
                RequestFailureKind::Definite
            },
            message: format!("{error:#}"),
            possible_orphan,
            status: None,
            retryable: true,
        }
    }
}

impl std::fmt::Display for BlobRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BlobRequestError {}

async fn upload_snapshot_with_timeout(
    server_url: &str,
    snapshot: &FileSnapshot,
    signer: &NgitSigner,
    total_timeout: Duration,
) -> std::result::Result<BlobUpload, BlobRequestError> {
    let deadline = tokio::time::Instant::now() + total_timeout;
    upload_snapshot_inner(server_url, snapshot, signer, deadline).await
}

async fn upload_snapshot_inner(
    server_url: &str,
    snapshot: &FileSnapshot,
    signer: &NgitSigner,
    deadline: tokio::time::Instant,
) -> std::result::Result<BlobUpload, BlobRequestError> {
    let event = tokio::time::timeout_at(
        deadline,
        upload_authorization(&[snapshot.sha256.as_str()], &[], signer),
    )
    .await
    .map_err(|_| {
        BlobRequestError::definite(
            anyhow!("Blossom upload authorization exceeded its total timeout"),
            false,
        )
    })?
    .map_err(|error| BlobRequestError::definite(error, false))?;
    let authorization =
        authorization_header(&event).map_err(|error| BlobRequestError::definite(error, false))?;
    let client = blossom_http_client().map_err(|error| BlobRequestError::definite(error, false))?;
    upload_snapshot_with_authorization_inner(
        &client,
        server_url,
        snapshot,
        authorization,
        deadline,
        None,
    )
    .await
}

#[derive(Clone)]
struct UploadProgressContext {
    reporter: Arc<dyn BlossomProgress>,
    batch: usize,
    batches: usize,
    server: Url,
    attempt: usize,
    preallocated: bool,
}

async fn upload_snapshot_with_authorization_inner(
    client: &reqwest::Client,
    server_url: &str,
    snapshot: &FileSnapshot,
    authorization: HeaderValue,
    deadline: tokio::time::Instant,
    progress: Option<UploadProgressContext>,
) -> std::result::Result<BlobUpload, BlobRequestError> {
    let upload_url = blossom_endpoint_url(server_url, "upload")
        .map_err(|error| BlobRequestError::definite(error, false))?;
    let file = snapshot
        .reopen()
        .map_err(|error| BlobRequestError::definite(error, false))?;
    if let Some(progress) = &progress {
        progress
            .reporter
            .update(&BlossomProgressEvent::UploadRequestStarted {
                batch: progress.batch,
                batches: progress.batches,
                filename: snapshot.filename.clone(),
                server: progress.server.clone(),
                attempt: progress.attempt,
                max_attempts: PLACEMENT_MAX_ATTEMPTS,
                total_bytes: snapshot.size,
                additional_bytes: if progress.preallocated {
                    0
                } else {
                    snapshot.size
                },
            });
    }
    let progress_for_stream = progress.clone();
    let streamed_filename = snapshot.filename.clone();
    let streamed_size = snapshot.size;
    let streamed = Arc::new(AtomicU64::new(0));
    let streamed_for_progress = streamed.clone();
    let (upload_activity, upload_activity_rx) = tokio::sync::mpsc::unbounded_channel();
    if streamed_size == 0 {
        if let Some(progress) = &progress {
            progress
                .reporter
                .update(&BlossomProgressEvent::UploadBodyFinished {
                    batch: progress.batch,
                    batches: progress.batches,
                    filename: streamed_filename.clone(),
                    server: progress.server.clone(),
                    attempt: progress.attempt,
                    max_attempts: PLACEMENT_MAX_ATTEMPTS,
                    idle_timeout_secs: IDLE_TIMEOUT.as_secs(),
                });
        }
    }
    let body_stream = ReaderStream::new(tokio::fs::File::from_std(file)).inspect(move |chunk| {
        if let (Some(progress), Ok(bytes)) = (&progress_for_stream, chunk) {
            let bytes = bytes.len().try_into().unwrap_or(u64::MAX);
            let previous = streamed_for_progress.fetch_add(bytes, Ordering::Relaxed);
            let _ = upload_activity.send(previous.saturating_add(bytes));
            progress
                .reporter
                .update(&BlossomProgressEvent::UploadedBytes {
                    batch: progress.batch,
                    filename: streamed_filename.clone(),
                    server: progress.server.clone(),
                    bytes,
                });
            if previous < streamed_size && previous.saturating_add(bytes) >= streamed_size {
                progress
                    .reporter
                    .update(&BlossomProgressEvent::UploadBodyFinished {
                        batch: progress.batch,
                        batches: progress.batches,
                        filename: streamed_filename.clone(),
                        server: progress.server.clone(),
                        attempt: progress.attempt,
                        max_attempts: PLACEMENT_MAX_ATTEMPTS,
                        idle_timeout_secs: IDLE_TIMEOUT.as_secs(),
                    });
            }
        }
    });
    let body = reqwest::Body::wrap_stream(body_stream);

    let request = client
        .put(upload_url)
        .header(CONTENT_LENGTH, snapshot.size)
        .header(CONTENT_TYPE, &snapshot.mime_type)
        .header("X-SHA-256", &snapshot.sha256)
        .header(AUTHORIZATION, authorization)
        .body(body);

    let response = send_upload_request_with_progress_timeout(
        request,
        upload_activity_rx,
        deadline,
        IDLE_TIMEOUT,
        snapshot.size,
    )
    .await?;

    let response_deadline = deadline.min(tokio::time::Instant::now() + IDLE_TIMEOUT);
    read_store_response(response, snapshot, response_deadline, "upload").await
}

async fn send_upload_request_with_progress_timeout(
    request: reqwest::RequestBuilder,
    mut activity: tokio::sync::mpsc::UnboundedReceiver<u64>,
    deadline: tokio::time::Instant,
    idle_timeout: Duration,
    total_bytes: u64,
) -> std::result::Result<reqwest::Response, BlobRequestError> {
    let request = request.send();
    tokio::pin!(request);
    let total_timer = tokio::time::sleep_until(deadline);
    tokio::pin!(total_timer);
    let idle_timer = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut activity_open = true;
    let mut uploaded_bytes = 0;

    loop {
        tokio::select! {
            response = &mut request => {
                return response.map_err(|error| classify_send_error(error, "upload", true));
            }
            _ = &mut total_timer => {
                return Err(BlobRequestError::unknown(
                    anyhow!("Blossom upload exceeded its 30 minute total timeout after sending {uploaded_bytes}/{total_bytes} bytes"),
                    true,
                ));
            }
            _ = &mut idle_timer => {
                return Err(BlobRequestError::unknown(
                    anyhow!("Blossom upload made no progress for 30 seconds after sending {uploaded_bytes}/{total_bytes} bytes"),
                    true,
                ));
            }
            next = activity.recv(), if activity_open => {
                if let Some(bytes) = next {
                    uploaded_bytes = uploaded_bytes.max(bytes);
                    idle_timer.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                } else {
                    activity_open = false;
                }
            }
        }
    }
}

async fn upload_snapshot_with_compatible_authorization(
    client: &reqwest::Client,
    server_url: &str,
    snapshot: &FileSnapshot,
    authorization: &CompatibleAuthorization,
    deadline: tokio::time::Instant,
    progress: UploadProgressContext,
) -> std::result::Result<BlobUpload, BlobRequestError> {
    let (primary_authorization, fallback_authorization) = match authorization.encoding_preference {
        AuthorizationEncodingPreference::Bud11 => (&authorization.bud11, &authorization.legacy),
        AuthorizationEncodingPreference::Legacy => (&authorization.legacy, &authorization.bud11),
    };
    let primary = upload_snapshot_with_authorization_inner(
        client,
        server_url,
        snapshot,
        primary_authorization.clone(),
        deadline,
        Some(progress.clone()),
    )
    .await;
    if primary.as_ref().is_err_and(auth_encoding_may_be_rejected) {
        return upload_snapshot_with_authorization_inner(
            client,
            server_url,
            snapshot,
            fallback_authorization.clone(),
            deadline,
            Some(UploadProgressContext {
                preallocated: false,
                ..progress
            }),
        )
        .await;
    }
    primary
}

fn auth_encoding_may_be_rejected(error: &BlobRequestError) -> bool {
    matches!(
        error.status,
        Some(StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND)
    )
}

enum BatchStoreConfirmation {
    Response(BlobUpload),
    Presence,
}

async fn upload_snapshot_with_confirmation(
    client: &reqwest::Client,
    server: &Url,
    snapshot: &FileSnapshot,
    authorization: &CompatibleAuthorization,
    progress: Arc<dyn BlossomProgress>,
    batch: usize,
    batches: usize,
) -> std::result::Result<BatchStoreConfirmation, BlobRequestError> {
    let mut last_error = None;
    for attempt in 0..PLACEMENT_MAX_ATTEMPTS {
        let deadline = tokio::time::Instant::now() + UPLOAD_REQUEST_TIMEOUT;
        match upload_snapshot_with_compatible_authorization(
            client,
            server.as_str(),
            snapshot,
            authorization,
            deadline,
            UploadProgressContext {
                reporter: progress.clone(),
                batch,
                batches,
                server: server.clone(),
                attempt: attempt + 1,
                preallocated: attempt == 0,
            },
        )
        .await
        {
            Ok(upload) => {
                match snapshot_is_present_with_retry_and_progress(
                    client, server, snapshot, &progress, batch, batches,
                )
                .await
                {
                    Ok(true) => return Ok(BatchStoreConfirmation::Response(upload)),
                    Ok(false) => {
                        last_error = Some(BlobRequestError::unknown(
                            anyhow!(
                                "Blossom upload was accepted but the blob was not queryable with matching metadata"
                            ),
                            true,
                        ));
                    }
                    Err(mut error) => {
                        error.kind = RequestFailureKind::Unknown;
                        error.possible_orphan = true;
                        error.message = format!(
                            "Blossom upload was accepted but post-upload verification failed: {}",
                            error.message
                        );
                        if !error.retryable {
                            return Err(error);
                        }
                        last_error = Some(error);
                    }
                }
            }
            Err(error) => {
                if error.possible_orphan {
                    if let Ok(true) = snapshot_is_present_with_retry_and_progress(
                        client, server, snapshot, &progress, batch, batches,
                    )
                    .await
                    {
                        return Ok(BatchStoreConfirmation::Presence);
                    }
                }
                if !error.retryable {
                    return Err(error);
                }
                last_error = Some(error);
            }
        }

        if attempt + 1 < PLACEMENT_MAX_ATTEMPTS {
            progress.update(&BlossomProgressEvent::RetryScheduled {
                batch,
                batches,
                filename: snapshot.filename.clone(),
                server: server.clone(),
                next_attempt: attempt + 2,
                max_attempts: PLACEMENT_MAX_ATTEMPTS,
            });
            tokio::time::sleep(placement_retry_delay(attempt)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| {
        BlobRequestError::definite(anyhow!("Blossom upload exhausted its retry plan"), false)
    }))
}

/// Confirm every unique snapshot on every server, uploading only missing
/// blobs. Upload authorizations cover batches of hashes and are scoped to all
/// selected server domains, so remote signers perform at most
/// `ceil(missing_hashes / 20)` authorization signatures.
///
/// `snapshots` must contain unique SHA-256 hashes. Every HEAD preflight
/// completes before an authorization is signed or a PUT is attempted. Upload
/// failures retain a complete outcome matrix and possible-orphan report.
/// Transient batch requests are retried with bounded backoff, and every
/// accepted or uncertain PUT is followed by a strict metadata presence check.
pub async fn upload_snapshot_batch_to_servers(
    servers: &[Url],
    snapshots: &[&FileSnapshot],
    signer: &NgitSigner,
    concurrency: usize,
) -> std::result::Result<BatchUploadResult, BatchUploadError> {
    upload_snapshot_batch_to_servers_with_progress(
        servers,
        snapshots,
        signer,
        concurrency,
        Arc::new(HiddenBlossomProgress),
    )
    .await
}

/// Batch placement variant which reports presence, authorization, upload,
/// verification, retry, and completion milestones.
pub async fn upload_snapshot_batch_to_servers_with_progress(
    servers: &[Url],
    snapshots: &[&FileSnapshot],
    signer: &NgitSigner,
    concurrency: usize,
    progress: Arc<dyn BlossomProgress>,
) -> std::result::Result<BatchUploadResult, BatchUploadError> {
    upload_snapshot_batch_to_servers_with_options_and_progress(
        servers,
        snapshots,
        signer,
        concurrency,
        progress,
        BatchAuthorizationOptions::DEFAULT,
    )
    .await
}

/// Batch placement which attempts every selected server while requiring one
/// confirmed copy of every blob. Standard multi-hash BUD-11 authorizations
/// retain signer batching while failed replicas remain visible in the result.
pub async fn upload_resilient_snapshot_batch_to_servers_with_progress(
    servers: &[Url],
    snapshots: &[&FileSnapshot],
    signer: &NgitSigner,
    concurrency: usize,
    progress: Arc<dyn BlossomProgress>,
) -> std::result::Result<BatchUploadResult, BatchUploadError> {
    upload_snapshot_batch_to_servers_with_options_and_progress(
        servers,
        snapshots,
        signer,
        concurrency,
        progress,
        BatchAuthorizationOptions::RESILIENT,
    )
    .await
}

/// Batch placement for software releases which attempts every selected server
/// while requiring one confirmed copy of every blob. It retains compatibility
/// with deployed servers that require padded Base64 and exactly one `x`
/// authorization tag.
pub async fn upload_release_snapshot_batch_to_servers_with_progress(
    servers: &[Url],
    snapshots: &[&FileSnapshot],
    signer: &NgitSigner,
    concurrency: usize,
    progress: Arc<dyn BlossomProgress>,
) -> std::result::Result<BatchUploadResult, BatchUploadError> {
    upload_snapshot_batch_to_servers_with_options_and_progress(
        servers,
        snapshots,
        signer,
        concurrency,
        progress,
        BatchAuthorizationOptions::LEGACY_PER_BLOB,
    )
    .await
}

async fn upload_snapshot_batch_to_servers_with_options_and_progress(
    servers: &[Url],
    snapshots: &[&FileSnapshot],
    signer: &NgitSigner,
    concurrency: usize,
    progress: Arc<dyn BlossomProgress>,
    authorization_options: BatchAuthorizationOptions,
) -> std::result::Result<BatchUploadResult, BatchUploadError> {
    if servers.is_empty() {
        return Err(empty_batch_error("at least one Blossom server is required"));
    }
    if snapshots.is_empty() {
        return Err(empty_batch_error(
            "at least one Blossom snapshot is required",
        ));
    }
    if concurrency == 0 {
        return Err(empty_batch_error(
            "Blossom upload concurrency must be greater than zero",
        ));
    }
    let mut seen_hashes = HashSet::new();
    if snapshots
        .iter()
        .any(|snapshot| !seen_hashes.insert(snapshot.sha256.as_str()))
    {
        return Err(empty_batch_error(
            "Blossom batch snapshots must have unique SHA-256 hashes",
        ));
    }

    let mut blobs = snapshots
        .iter()
        .map(|snapshot| BatchBlobUploadOutcome {
            sha256: snapshot.sha256.clone(),
            servers: servers
                .iter()
                .map(|server| BlossomServerOutcome {
                    server: server.clone(),
                    operation: BlossomServerOperation::Upload,
                    status: BlossomServerStatus::NotAttempted,
                    descriptor: None,
                    message: None,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let client = blossom_streaming_http_client().map_err(|error| BatchUploadError {
        message: format!("failed to prepare Blossom batch client: {error:#}"),
        blobs: blobs.clone(),
        possible_orphan_blobs: Vec::new(),
    })?;

    progress.update(&BlossomProgressEvent::PresenceChecksStarted {
        blobs: snapshots.len(),
        servers: servers.len(),
        checks: snapshots.len().saturating_mul(servers.len()),
    });
    let presence_circuits = servers
        .iter()
        .map(|_| Arc::new(PresenceCircuit::default()))
        .collect::<Vec<_>>();
    let presence_checks = snapshots
        .iter()
        .enumerate()
        .flat_map(|(blob_index, snapshot)| {
            let presence_circuits = &presence_circuits;
            servers
                .iter()
                .enumerate()
                .map(move |(server_index, server)| {
                    (
                        blob_index,
                        server_index,
                        *snapshot,
                        server,
                        Arc::clone(&presence_circuits[server_index]),
                    )
                })
        });
    let mut presence_results = stream::iter(presence_checks)
        .map(|(blob_index, server_index, snapshot, server, circuit)| {
            let client = client.clone();
            async move {
                (
                    blob_index,
                    server_index,
                    snapshot_is_present_with_retry_on_circuit(&client, server, snapshot, &circuit)
                        .await,
                )
            }
        })
        .buffer_unordered(concurrency);

    let mut presence_failed = false;
    let mut missing = Vec::new();
    let mut checked = 0_usize;
    let mut confirmed = 0_usize;
    let mut unavailable = 0_usize;
    while let Some((blob_index, server_index, result)) = presence_results.next().await {
        match result {
            PresenceProbeResult::Checked(Ok(true)) => {
                confirmed += 1;
                blobs[blob_index].servers[server_index].status =
                    BlossomServerStatus::AlreadyPresent;
                progress.update(&BlossomProgressEvent::PlacementFinished {
                    filename: snapshots[blob_index].filename.clone(),
                    server: servers[server_index].clone(),
                    status: BlossomServerStatus::AlreadyPresent,
                    message: None,
                });
            }
            PresenceProbeResult::Checked(Ok(false)) => {
                missing.push((blob_index, server_index));
            }
            PresenceProbeResult::Checked(Err(error)) => {
                unavailable += 1;
                presence_failed = true;
                let outcome = &mut blobs[blob_index].servers[server_index];
                outcome.status = match error.kind {
                    RequestFailureKind::Definite => BlossomServerStatus::Failed,
                    RequestFailureKind::Unknown => BlossomServerStatus::Unknown,
                };
                outcome.message = Some(error.message);
                progress.update(&BlossomProgressEvent::PlacementFinished {
                    filename: snapshots[blob_index].filename.clone(),
                    server: servers[server_index].clone(),
                    status: outcome.status,
                    message: outcome.message.clone(),
                });
            }
            PresenceProbeResult::Skipped => {
                unavailable += 1;
                presence_failed = true;
                let outcome = &mut blobs[blob_index].servers[server_index];
                outcome.message = Some(format!(
                    "presence check skipped after {PRESENCE_CIRCUIT_FAILURE_THRESHOLD} consecutive transient failures on this server"
                ));
                progress.update(&BlossomProgressEvent::PlacementFinished {
                    filename: snapshots[blob_index].filename.clone(),
                    server: servers[server_index].clone(),
                    status: outcome.status,
                    message: outcome.message.clone(),
                });
            }
        }
        checked += 1;
        progress.update(&BlossomProgressEvent::PresenceCheckFinished {
            checked,
            checks: snapshots.len().saturating_mul(servers.len()),
            confirmed,
            missing: missing.len(),
            unavailable,
        });
    }
    progress.update(&BlossomProgressEvent::PresenceChecksFinished {
        missing: missing.len(),
    });
    if presence_failed
        && authorization_options.placement_requirement == PlacementRequirement::EveryServer
    {
        return Err(BatchUploadError {
            message:
                "one or more Blossom presence checks failed; no upload authorization was signed"
                    .to_owned(),
            blobs,
            possible_orphan_blobs: Vec::new(),
        });
    }
    if missing.is_empty() {
        return finish_batch_for_requirement(
            blobs,
            authorization_options.placement_requirement,
            Vec::new(),
        );
    }

    let missing_blob_indices = missing
        .iter()
        .map(|(blob_index, _)| *blob_index)
        .collect::<BTreeSet<_>>();
    let missing_hashes = missing_blob_indices
        .into_iter()
        .map(|blob_index| snapshots[blob_index].sha256.as_str())
        .collect::<Vec<_>>();
    let authorization_batches = missing_hashes
        .len()
        .div_ceil(authorization_options.batch_size);
    let mut upload_failed = false;
    let mut uncertain = Vec::new();
    for (batch_index, hashes) in missing_hashes
        .chunks(authorization_options.batch_size)
        .enumerate()
    {
        let batch = batch_index + 1;
        let hashes_in_chunk = hashes.iter().copied().collect::<HashSet<_>>();
        let chunk_blob_indices = snapshots
            .iter()
            .enumerate()
            .filter(|(_, snapshot)| hashes_in_chunk.contains(snapshot.sha256.as_str()))
            .map(|(blob_index, _)| blob_index)
            .collect::<Vec<_>>();
        let filenames = chunk_blob_indices
            .iter()
            .map(|blob_index| snapshots[*blob_index].filename.clone())
            .collect::<Vec<_>>();
        let uploads = missing
            .iter()
            .copied()
            .filter(|(blob_index, _)| {
                hashes_in_chunk.contains(snapshots[*blob_index].sha256.as_str())
            })
            .collect::<Vec<_>>();
        let upload_window = batch_upload_window(uploads.len(), concurrency).map_err(|error| {
            batch_progress_error(
                format!("failed to schedule Blossom batch upload: {error:#}"),
                &blobs,
            )
        })?;
        let required_remaining = upload_window
            .checked_add(AUTHORIZATION_LIFETIME)
            .ok_or_else(|| {
                batch_progress_error(
                    "Blossom batch authorization lifetime overflowed".to_owned(),
                    &blobs,
                )
            })?;
        let authorization_lifetime = required_remaining
            .checked_add(AUTHORIZATION_SIGNING_ALLOWANCE)
            .ok_or_else(|| {
                batch_progress_error(
                    "Blossom batch authorization lifetime overflowed".to_owned(),
                    &blobs,
                )
            })?;
        progress.update(&BlossomProgressEvent::AuthorizationStarted {
            batch,
            batches: authorization_batches,
            blobs: hashes.len(),
            filenames: filenames.clone(),
        });
        let (event, expires) =
            upload_authorization_with_lifetime(hashes, servers, signer, authorization_lifetime)
                .await
                .map_err(|error| {
                    batch_progress_error(
                        format!("failed to authorize Blossom batch upload: {error:#}"),
                        &blobs,
                    )
                })?;
        if Timestamp::now() + required_remaining >= expires {
            return Err(batch_progress_error(
                "Blossom batch authorization expired while waiting for the signer; no upload used the stale token"
                    .to_owned(),
                &blobs,
            ));
        }
        let authorization =
            compatible_authorization_headers(&event, authorization_options.encoding_preference)
                .map_err(|error| {
                    batch_progress_error(
                        format!("failed to encode Blossom batch authorization: {error:#}"),
                        &blobs,
                    )
                })?;
        let upload_bytes = uploads.iter().fold(0_u64, |total, (blob_index, _)| {
            total.saturating_add(snapshots[*blob_index].size)
        });
        let total_placements = chunk_blob_indices.len().saturating_mul(servers.len());
        let confirmed = chunk_blob_indices
            .iter()
            .flat_map(|blob_index| blobs[*blob_index].servers.iter())
            .filter(|outcome| server_outcome_is_confirmed(outcome))
            .count();
        let unavailable = total_placements
            .saturating_sub(confirmed)
            .saturating_sub(uploads.len());
        progress.update(&BlossomProgressEvent::UploadBatchStarted {
            batch,
            batches: authorization_batches,
            blobs: hashes.len(),
            filenames,
            placements: uploads.len(),
            confirmed,
            unavailable,
            bytes: upload_bytes,
        });
        let mut upload_results = stream::iter(uploads)
            .map(|(blob_index, server_index)| {
                let client = client.clone();
                let authorization = authorization.clone();
                let snapshot = snapshots[blob_index];
                let server = &servers[server_index];
                let progress = progress.clone();
                async move {
                    (
                        blob_index,
                        server_index,
                        upload_snapshot_with_confirmation(
                            &client,
                            server,
                            snapshot,
                            &authorization,
                            progress,
                            batch,
                            authorization_batches,
                        )
                        .await,
                    )
                }
            })
            .buffer_unordered(concurrency);

        let mut chunk_failed = false;
        while let Some((blob_index, server_index, result)) = upload_results.next().await {
            let sha256 = blobs[blob_index].sha256.clone();
            let outcome = &mut blobs[blob_index].servers[server_index];
            match result {
                Ok(BatchStoreConfirmation::Response(upload)) => {
                    record_server_success(outcome, &upload);
                }
                Ok(BatchStoreConfirmation::Presence) => {
                    outcome.status = BlossomServerStatus::Stored;
                }
                Err(error) => {
                    chunk_failed = true;
                    outcome.status = match error.kind {
                        RequestFailureKind::Definite => BlossomServerStatus::Failed,
                        RequestFailureKind::Unknown => BlossomServerStatus::Unknown,
                    };
                    outcome.message = Some(error.message);
                    if error.possible_orphan {
                        uncertain.push(PossibleOrphanBlob {
                            server: outcome.server.clone(),
                            sha256,
                            url: None,
                        });
                    }
                }
            }
            progress.update(&BlossomProgressEvent::PlacementFinished {
                filename: snapshots[blob_index].filename.clone(),
                server: servers[server_index].clone(),
                status: outcome.status,
                message: outcome.message.clone(),
            });
        }
        progress.update(&BlossomProgressEvent::UploadBatchFinished {
            batch,
            batches: authorization_batches,
        });
        if chunk_failed
            && authorization_options.placement_requirement == PlacementRequirement::EveryServer
        {
            upload_failed = true;
            break;
        }
    }
    if upload_failed {
        let mut possible_orphan_blobs = stored_batch_blobs(&blobs);
        possible_orphan_blobs.extend(uncertain);
        return Err(BatchUploadError {
            message: "one or more Blossom uploads failed; no publication event was signed"
                .to_owned(),
            blobs,
            possible_orphan_blobs,
        });
    }

    finish_batch_for_requirement(
        blobs,
        authorization_options.placement_requirement,
        uncertain,
    )
}

fn finish_batch_for_requirement(
    blobs: Vec<BatchBlobUploadOutcome>,
    requirement: PlacementRequirement,
    uncertain: Vec<PossibleOrphanBlob>,
) -> std::result::Result<BatchUploadResult, BatchUploadError> {
    let unavailable = blobs
        .iter()
        .filter(|blob| !blob.servers.iter().any(server_outcome_is_confirmed))
        .count();
    if unavailable != 0 {
        let mut possible_orphan_blobs = stored_batch_blobs(&blobs);
        possible_orphan_blobs.extend(uncertain);
        return Err(BatchUploadError {
            message: format!(
                "{unavailable}/{} Blossom blobs were not confirmed on any selected server; no publication event was signed",
                blobs.len()
            ),
            blobs,
            possible_orphan_blobs,
        });
    }
    if requirement == PlacementRequirement::EveryServer
        && blobs.iter().any(|blob| {
            blob.servers
                .iter()
                .any(|outcome| !server_outcome_is_confirmed(outcome))
        })
    {
        let mut possible_orphan_blobs = stored_batch_blobs(&blobs);
        possible_orphan_blobs.extend(uncertain);
        return Err(BatchUploadError {
            message: "one or more Blossom placements failed; no publication event was signed"
                .to_owned(),
            blobs,
            possible_orphan_blobs,
        });
    }
    Ok(BatchUploadResult { blobs })
}

fn server_outcome_is_confirmed(outcome: &BlossomServerOutcome) -> bool {
    matches!(
        outcome.status,
        BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent
    )
}

fn empty_batch_error(message: &str) -> BatchUploadError {
    BatchUploadError {
        message: message.to_owned(),
        blobs: Vec::new(),
        possible_orphan_blobs: Vec::new(),
    }
}

fn batch_upload_window(operation_count: usize, concurrency: usize) -> Result<Duration> {
    if operation_count == 0 || concurrency == 0 {
        bail!("a Blossom upload batch requires work and non-zero concurrency");
    }
    let waves = operation_count.div_ceil(concurrency);
    let waves = u64::try_from(waves).context("Blossom upload wave count does not fit in u64")?;
    let attempts =
        u32::try_from(PLACEMENT_MAX_ATTEMPTS).context("Blossom retry count does not fit in u32")?;
    // Each upload attempt has one PUT window followed by up to `attempts`
    // verification HEAD windows. Keep the authorization valid for every
    // bounded operation even though normal successful uploads use only two.
    let upload_window = UPLOAD_REQUEST_TIMEOUT
        .checked_mul(attempts)
        .context("Blossom upload window overflowed")?;
    let presence_windows = attempts
        .checked_mul(attempts)
        .context("Blossom upload request window count overflowed")?;
    let presence_window = PRESENCE_REQUEST_TIMEOUT
        .checked_mul(presence_windows)
        .context("Blossom presence-check window overflowed")?;
    let operation_window = upload_window
        .checked_add(presence_window)
        .context("Blossom upload operation window overflowed")?;
    operation_window
        .checked_mul(u32::try_from(waves).context("Blossom upload wave count is too large")?)
        .context("Blossom upload window overflowed")
}

fn placement_retry_delay(attempt: usize) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.try_into().unwrap_or(u32::MAX));
    multiplier
        .and_then(|multiplier| PLACEMENT_RETRY_BASE_DELAY.checked_mul(multiplier))
        .unwrap_or(PLACEMENT_RETRY_BASE_DELAY)
}

fn stored_batch_blobs(blobs: &[BatchBlobUploadOutcome]) -> Vec<PossibleOrphanBlob> {
    blobs
        .iter()
        .flat_map(|blob| {
            blob.servers
                .iter()
                .filter(|outcome| outcome.status == BlossomServerStatus::Stored)
                .map(|outcome| PossibleOrphanBlob {
                    server: outcome.server.clone(),
                    sha256: blob.sha256.clone(),
                    url: outcome
                        .descriptor
                        .as_ref()
                        .map(|descriptor| descriptor.url.clone()),
                })
        })
        .collect()
}

fn batch_progress_error(message: String, blobs: &[BatchBlobUploadOutcome]) -> BatchUploadError {
    BatchUploadError {
        message,
        blobs: blobs.to_vec(),
        possible_orphan_blobs: stored_batch_blobs(blobs),
    }
}

async fn snapshot_is_present(
    client: &reqwest::Client,
    server: &Url,
    snapshot: &FileSnapshot,
) -> std::result::Result<bool, BlobRequestError> {
    snapshot_is_present_once(client, server, snapshot, PRESENCE_REQUEST_TIMEOUT).await
}

async fn snapshot_is_present_with_retry_on_circuit(
    client: &reqwest::Client,
    server: &Url,
    snapshot: &FileSnapshot,
    circuit: &PresenceCircuit,
) -> PresenceProbeResult {
    snapshot_is_present_with_retry_notifying(client, server, snapshot, Some(circuit), |_| {}).await
}

async fn snapshot_is_present_with_retry_and_progress(
    client: &reqwest::Client,
    server: &Url,
    snapshot: &FileSnapshot,
    progress: &Arc<dyn BlossomProgress>,
    batch: usize,
    batches: usize,
) -> std::result::Result<bool, BlobRequestError> {
    match snapshot_is_present_with_retry_notifying(client, server, snapshot, None, |attempt| {
        progress.update(&BlossomProgressEvent::VerificationStarted {
            batch,
            batches,
            filename: snapshot.filename.clone(),
            server: server.clone(),
            attempt,
            max_attempts: PLACEMENT_MAX_ATTEMPTS,
            timeout_secs: PRESENCE_REQUEST_TIMEOUT.as_secs(),
        });
    })
    .await
    {
        PresenceProbeResult::Checked(result) => result,
        PresenceProbeResult::Skipped => {
            unreachable!("upload verification without a circuit cannot be skipped")
        }
    }
}

async fn snapshot_is_present_with_retry_notifying(
    client: &reqwest::Client,
    server: &Url,
    snapshot: &FileSnapshot,
    circuit: Option<&PresenceCircuit>,
    mut notify_attempt: impl FnMut(usize),
) -> PresenceProbeResult {
    let mut last_error = None;
    for attempt in 0..PLACEMENT_MAX_ATTEMPTS {
        if circuit.is_some_and(PresenceCircuit::is_open) {
            return last_error.map_or(PresenceProbeResult::Skipped, |error| {
                PresenceProbeResult::Checked(Err(error))
            });
        }
        notify_attempt(attempt + 1);
        match snapshot_is_present(client, server, snapshot).await {
            Ok(present) => {
                if let Some(circuit) = circuit {
                    circuit.record_success();
                }
                return PresenceProbeResult::Checked(Ok(present));
            }
            Err(error) if error.retryable => {
                let opened = circuit.is_some_and(PresenceCircuit::record_transient_failure);
                last_error = Some(error);
                if opened {
                    break;
                }
            }
            Err(error) => return PresenceProbeResult::Checked(Err(error)),
        }
        if attempt + 1 < PLACEMENT_MAX_ATTEMPTS {
            tokio::time::sleep(placement_retry_delay(attempt)).await;
        }
    }
    PresenceProbeResult::Checked(Err(last_error.unwrap_or_else(|| {
        BlobRequestError::definite(
            anyhow!("Blossom presence check exhausted its retry plan"),
            false,
        )
    })))
}

async fn snapshot_is_present_once(
    client: &reqwest::Client,
    server: &Url,
    snapshot: &FileSnapshot,
    total_timeout: Duration,
) -> std::result::Result<bool, BlobRequestError> {
    let mut url = blossom_endpoint_url(server.as_str(), &snapshot.sha256)
        .map_err(|error| BlobRequestError::definite(error, false))?;
    let deadline = tokio::time::Instant::now() + total_timeout;
    for redirects in 0..=MAX_PRESENCE_REDIRECTS {
        let response = tokio::time::timeout_at(deadline, client.head(url.clone()).send())
            .await
            .map_err(|_| {
                BlobRequestError::transport(
                    anyhow!("Blossom presence check exceeded its total timeout"),
                    false,
                )
            })?
            .map_err(|error| classify_send_error(error, "presence check", false))?;
        match response.status() {
            StatusCode::OK => {
                validate_presence_metadata(&response, snapshot)
                    .map_err(|error| BlobRequestError::definite(error, false))?;
                return Ok(true);
            }
            StatusCode::NOT_FOUND => return Ok(false),
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                if redirects == MAX_PRESENCE_REDIRECTS {
                    return Err(BlobRequestError::definite(
                        anyhow!(
                            "Blossom presence check exceeded {MAX_PRESENCE_REDIRECTS} redirects"
                        ),
                        false,
                    ));
                }
                url = presence_redirect_url(&response, &snapshot.sha256)
                    .map_err(|error| BlobRequestError::definite(error, false))?;
            }
            status => {
                return Err(BlobRequestError::http(
                    anyhow!("Blossom presence check returned HTTP {status}"),
                    status,
                    RequestFailureKind::Definite,
                    false,
                ));
            }
        }
    }
    unreachable!("the bounded redirect loop always returns")
}

fn validate_presence_metadata(response: &reqwest::Response, snapshot: &FileSnapshot) -> Result<()> {
    let content_length = response
        .headers()
        .get(CONTENT_LENGTH)
        .context("Blossom presence response omitted Content-Length")?
        .to_str()
        .context("Blossom presence response returned a non-text Content-Length")?
        .parse::<u64>()
        .context("Blossom presence response returned an invalid Content-Length")?;
    if content_length != snapshot.size {
        bail!(
            "Blossom presence response Content-Length {content_length} does not match the {} byte snapshot",
            snapshot.size
        );
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .context("Blossom presence response omitted Content-Type")?
        .to_str()
        .context("Blossom presence response returned a non-text Content-Type")?;
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if !snapshot_media_type_matches(snapshot, media_type) {
        bail!(
            "Blossom presence response Content-Type {content_type:?} does not match snapshot MIME type {:?}",
            snapshot.mime_type
        );
    }
    Ok(())
}

fn presence_redirect_url(response: &reqwest::Response, sha256: &str) -> Result<Url> {
    let location = response
        .headers()
        .get(LOCATION)
        .context("Blossom presence redirect omitted Location")?
        .to_str()
        .context("Blossom presence redirect returned a non-text Location")?;
    let redirect = response
        .url()
        .join(location)
        .context("Blossom presence redirect Location is invalid")?;
    if !matches!(redirect.scheme(), "http" | "https") || redirect.host_str().is_none() {
        bail!("Blossom presence redirect must target an absolute HTTP or HTTPS URL");
    }
    if !redirect.username().is_empty() || redirect.password().is_some() {
        bail!("Blossom presence redirect must not contain embedded credentials");
    }
    let mut comparable = redirect.clone();
    comparable.set_fragment(None);
    if !comparable.as_str().contains(sha256) {
        bail!("Blossom presence redirect does not contain the requested SHA-256");
    }
    Ok(redirect)
}

/// Confirm one snapshot on every selected server using the shared placement
/// engine.
///
/// All servers are checked with strict BUD-01 metadata before the signer is
/// invoked. Missing copies use batched BUD-11-compatible authorization,
/// bounded concurrency and retries, and post-upload HEAD verification. The
/// result retains one outcome per selected server in caller-provided order.
pub async fn confirm_snapshot_on_servers(
    servers: &[Url],
    snapshot: &FileSnapshot,
    signer: &NgitSigner,
) -> std::result::Result<MultiServerUpload, MultiServerUploadError> {
    let batch =
        upload_snapshot_batch_to_servers(servers, &[snapshot], signer, DEFAULT_UPLOAD_CONCURRENCY)
            .await
            .map_err(multi_server_error_from_batch)?;
    let Some(blob) = batch.blobs.first() else {
        return Err(MultiServerUploadError {
            message: "Blossom placement returned no outcome for the requested snapshot".to_owned(),
            servers: Vec::new(),
            possible_orphan_blobs: Vec::new(),
        });
    };
    multi_server_upload_from_batch_outcome(snapshot, blob)
}

/// Convert one successful batch row into the single-blob result used by
/// release and container publication output.
pub fn multi_server_upload_from_batch_outcome(
    snapshot: &FileSnapshot,
    blob: &BatchBlobUploadOutcome,
) -> std::result::Result<MultiServerUpload, MultiServerUploadError> {
    let Some(primary_outcome) = blob
        .servers
        .iter()
        .find(|outcome| server_outcome_is_confirmed(outcome))
    else {
        return Err(MultiServerUploadError {
            message: "Blossom placement returned no confirmed server outcome".to_owned(),
            servers: blob.servers.clone(),
            possible_orphan_blobs: Vec::new(),
        });
    };
    let primary = if let Some(descriptor) = primary_outcome.descriptor.clone() {
        descriptor
    } else {
        let url = blossom_endpoint_url(primary_outcome.server.as_str(), &snapshot.sha256).map_err(
            |error| MultiServerUploadError {
                message: format!("failed to construct verified Blossom blob URL: {error:#}"),
                servers: blob.servers.clone(),
                possible_orphan_blobs: stored_batch_blobs(std::slice::from_ref(blob)),
            },
        )?;
        BlobDescriptor {
            url,
            sha256: snapshot.sha256.clone(),
            size: snapshot.size,
            mime_type: snapshot.mime_type.clone(),
            uploaded: 0,
        }
    };
    Ok(MultiServerUpload {
        primary,
        servers: blob.servers.clone(),
    })
}

fn multi_server_error_from_batch(error: BatchUploadError) -> MultiServerUploadError {
    MultiServerUploadError {
        message: error.message,
        servers: error
            .blobs
            .into_iter()
            .next()
            .map(|blob| blob.servers)
            .unwrap_or_default(),
        possible_orphan_blobs: error.possible_orphan_blobs,
    }
}

/// Upload to the first server, then mirror sequentially to every other server.
///
/// This low-level BUD-04 primitive remains available for callers that
/// explicitly want remote mirroring. ngit's publication workflows use
/// [`confirm_snapshot_on_servers`] so every selected server is independently
/// verified before an event is signed.
///
/// The first failure stops network activity. The returned error still contains
/// every planned server in caller-provided order and marks untouched suffixes
/// as `not_attempted` so callers do not have to infer partial completion.
pub async fn upload_snapshot_to_servers(
    servers: &[Url],
    snapshot: &FileSnapshot,
    signer: &NgitSigner,
) -> std::result::Result<MultiServerUpload, MultiServerUploadError> {
    let mut outcomes = servers
        .iter()
        .enumerate()
        .map(|(index, server)| BlossomServerOutcome {
            server: server.clone(),
            operation: if index == 0 {
                BlossomServerOperation::Upload
            } else {
                BlossomServerOperation::Mirror
            },
            status: BlossomServerStatus::NotAttempted,
            descriptor: None,
            message: None,
        })
        .collect::<Vec<_>>();
    let Some(primary_server) = servers.first() else {
        return Err(MultiServerUploadError {
            message: "at least one Blossom server is required".to_owned(),
            servers: outcomes,
            possible_orphan_blobs: Vec::new(),
        });
    };

    let primary = match upload_snapshot_with_timeout(
        primary_server.as_str(),
        snapshot,
        signer,
        TOTAL_TIMEOUT,
    )
    .await
    {
        Ok(upload) => {
            record_server_success(&mut outcomes[0], &upload);
            upload.descriptor
        }
        Err(error) => return Err(record_server_failure(outcomes, 0, snapshot, error)),
    };

    for index in 1..servers.len() {
        match mirror_snapshot_with_timeout(
            servers[index].as_str(),
            &primary.url,
            snapshot,
            signer,
            TOTAL_TIMEOUT,
        )
        .await
        {
            Ok(upload) => record_server_success(&mut outcomes[index], &upload),
            Err(error) => {
                return Err(record_server_failure(outcomes, index, snapshot, error));
            }
        }
    }

    Ok(MultiServerUpload {
        primary,
        servers: outcomes,
    })
}

fn record_server_success(outcome: &mut BlossomServerOutcome, upload: &BlobUpload) {
    outcome.status = if upload.newly_stored {
        BlossomServerStatus::Stored
    } else {
        BlossomServerStatus::AlreadyPresent
    };
    outcome.descriptor = Some(upload.descriptor.clone());
}

fn record_server_failure(
    mut outcomes: Vec<BlossomServerOutcome>,
    failed_index: usize,
    snapshot: &FileSnapshot,
    error: BlobRequestError,
) -> MultiServerUploadError {
    let operation = outcomes[failed_index].operation;
    let message = format!(
        "Blossom {} at {} failed: {}",
        operation_label(operation),
        outcomes[failed_index].server,
        error.message
    );
    outcomes[failed_index].status = match error.kind {
        RequestFailureKind::Definite => BlossomServerStatus::Failed,
        RequestFailureKind::Unknown => BlossomServerStatus::Unknown,
    };
    outcomes[failed_index].message = Some(error.message);

    let mut possible_orphan_blobs = outcomes
        .iter()
        .filter(|outcome| outcome.status == BlossomServerStatus::Stored)
        .map(|outcome| PossibleOrphanBlob {
            server: outcome.server.clone(),
            sha256: snapshot.sha256.clone(),
            url: outcome
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.url.clone()),
        })
        .collect::<Vec<_>>();
    if error.possible_orphan {
        possible_orphan_blobs.push(PossibleOrphanBlob {
            server: outcomes[failed_index].server.clone(),
            sha256: snapshot.sha256.clone(),
            url: None,
        });
    }

    MultiServerUploadError {
        message,
        servers: outcomes,
        possible_orphan_blobs,
    }
}

fn operation_label(operation: BlossomServerOperation) -> &'static str {
    match operation {
        BlossomServerOperation::Upload => "upload",
        BlossomServerOperation::Mirror => "mirror",
    }
}

async fn mirror_snapshot_with_timeout(
    server_url: &str,
    primary_url: &Url,
    snapshot: &FileSnapshot,
    signer: &NgitSigner,
    total_timeout: Duration,
) -> std::result::Result<BlobUpload, BlobRequestError> {
    let deadline = tokio::time::Instant::now() + total_timeout;
    let mirror_url = blossom_endpoint_url(server_url, "mirror")
        .map_err(|error| BlobRequestError::definite(error, false))?;
    validate_blob_url(primary_url, &snapshot.sha256)
        .context("invalid Blossom mirror source URL")
        .map_err(|error| BlobRequestError::definite(error, false))?;
    let event = tokio::time::timeout_at(
        deadline,
        upload_authorization(&[snapshot.sha256.as_str()], &[], signer),
    )
    .await
    .map_err(|_| {
        BlobRequestError::definite(
            anyhow!("Blossom mirror authorization exceeded its total timeout"),
            false,
        )
    })?
    .map_err(|error| BlobRequestError::definite(error, false))?;
    let authorization =
        authorization_header(&event).map_err(|error| BlobRequestError::definite(error, false))?;
    let client = blossom_http_client().map_err(|error| BlobRequestError::definite(error, false))?;
    let request = client
        .put(mirror_url)
        .header("X-SHA-256", &snapshot.sha256)
        .header("X-Content-Length", snapshot.size)
        .header("X-Content-Type", &snapshot.mime_type)
        .header(AUTHORIZATION, authorization)
        .json(&serde_json::json!({ "url": primary_url }));

    let response = tokio::time::timeout_at(deadline, request.send())
        .await
        .map_err(|_| {
            BlobRequestError::unknown(anyhow!("Blossom mirror exceeded its total timeout"), true)
        })?
        .map_err(|error| classify_send_error(error, "mirror", true))?;

    read_store_response(response, snapshot, deadline, "mirror").await
}

fn classify_send_error(
    error: reqwest::Error,
    operation: &str,
    may_have_stored: bool,
) -> BlobRequestError {
    let definitely_not_stored = error.is_builder() || error.is_connect();
    let error = anyhow!(error).context(format!("failed to send the Blossom {operation} request"));
    BlobRequestError::transport(error, may_have_stored && !definitely_not_stored)
}

fn is_transient_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn blossom_http_client() -> Result<reqwest::Client> {
    crate::tls::http_client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .context("failed to create the Blossom HTTP client")
}

fn blossom_streaming_http_client() -> Result<reqwest::Client> {
    crate::tls::http_client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .context("failed to create the streaming Blossom HTTP client")
}

async fn read_store_response(
    mut response: reqwest::Response,
    snapshot: &FileSnapshot,
    deadline: tokio::time::Instant,
    operation: &str,
) -> std::result::Result<BlobUpload, BlobRequestError> {
    let status = response.status();
    if status != StatusCode::OK && status != StatusCode::CREATED {
        let body = read_error_response_snippet(&mut response, deadline).await;
        let guidance = match status {
            StatusCode::UNAUTHORIZED => {
                "authentication challenge flows are not supported beyond the BUD-11 compatibility fallback"
            }
            StatusCode::PAYMENT_REQUIRED => {
                "paid Blossom uploads are not supported; choose a server which accepts this blob"
            }
            status if status.is_server_error() => {
                "the server failed while handling the request; blob storage is uncertain"
            }
            _ => "the Blossom server rejected the request",
        };
        let error = anyhow!("Blossom {operation} returned HTTP {status}: {guidance}{body}");
        return Err(if status.is_server_error() {
            BlobRequestError::http(error, status, RequestFailureKind::Unknown, true)
        } else {
            BlobRequestError::http(error, status, RequestFailureKind::Definite, false)
        });
    }
    let possible_orphan = status == StatusCode::CREATED;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DESCRIPTOR_BYTES)
    {
        return Err(BlobRequestError::definite(
            anyhow!("Blossom descriptor exceeds the response size limit"),
            possible_orphan,
        ));
    }

    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| {
                BlobRequestError::unknown(
                    anyhow!("Blossom {operation} exceeded its total timeout"),
                    possible_orphan,
                )
            })?
            .map_err(|error| {
                BlobRequestError::unknown(
                    anyhow!(error).context("failed while reading the Blossom descriptor"),
                    possible_orphan,
                )
            })?;
        let Some(chunk) = chunk else { break };
        let length = u64::try_from(bytes.len())
            .ok()
            .and_then(|length| length.checked_add(u64::try_from(chunk.len()).ok()?))
            .ok_or_else(|| {
                BlobRequestError::definite(
                    anyhow!("Blossom descriptor length overflowed u64"),
                    possible_orphan,
                )
            })?;
        if length > MAX_DESCRIPTOR_BYTES {
            return Err(BlobRequestError::definite(
                anyhow!("Blossom descriptor exceeds the response size limit"),
                possible_orphan,
            ));
        }
        bytes.extend_from_slice(&chunk);
    }

    let descriptor: BlobDescriptor = serde_json::from_slice(&bytes).map_err(|error| {
        BlobRequestError::definite(
            anyhow!(error).context("Blossom server returned an invalid blob descriptor"),
            possible_orphan,
        )
    })?;
    validate_descriptor(&descriptor, snapshot)
        .map_err(|error| BlobRequestError::definite(error, possible_orphan))?;
    Ok(BlobUpload {
        descriptor,
        newly_stored: status == StatusCode::CREATED,
    })
}

async fn read_error_response_snippet(
    response: &mut reqwest::Response,
    deadline: tokio::time::Instant,
) -> String {
    if response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("text/html"))
        })
    {
        return "; server returned an HTML error page instead of a Blossom response".to_owned();
    }
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BODY_BYTES {
        let chunk = match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            Ok(Err(_)) => return " (response body could not be read)".to_owned(),
            Err(_) => return " (response body timed out)".to_owned(),
        };
        let remaining = MAX_ERROR_BODY_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            break;
        }
    }
    let body = terminal_safe_remote_text(&String::from_utf8_lossy(&bytes))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if body.is_empty() {
        String::new()
    } else {
        format!("; server response: {body}")
    }
}

fn terminal_safe_remote_text(input: &str) -> String {
    input
        .chars()
        .filter_map(|character| {
            if character.is_whitespace() {
                Some(' ')
            } else if character.is_control() || is_bidi_control(character) {
                None
            } else {
                Some(character)
            }
        })
        .collect()
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

async fn upload_authorization(
    hashes: &[&str],
    servers: &[Url],
    signer: &NgitSigner,
) -> Result<Event> {
    upload_authorization_with_lifetime(hashes, servers, signer, AUTHORIZATION_LIFETIME)
        .await
        .map(|(event, _)| event)
}

async fn upload_authorization_with_lifetime(
    hashes: &[&str],
    servers: &[Url],
    signer: &NgitSigner,
    lifetime: Duration,
) -> Result<(Event, Timestamp)> {
    if hashes.is_empty() {
        bail!("Blossom upload authorization requires at least one hash");
    }
    let expires = Timestamp::now() + lifetime;
    let mut tags = vec![Tag::parse(["t", "upload"]).expect("static Blossom action tag is valid")];
    for hash in hashes {
        tags.push(Tag::parse(["x", *hash]).context("invalid Blossom upload hash tag")?);
    }
    let mut seen_servers = HashSet::new();
    for server in servers {
        let domain = server
            .host_str()
            .context("Blossom authorization server has no domain")?
            .to_ascii_lowercase();
        if seen_servers.insert(domain.clone()) {
            tags.push(
                Tag::parse(["server", domain.as_str()])
                    .context("invalid Blossom authorization server tag")?,
            );
        }
    }
    tags.push(Tag::expiration(expires));
    let content = if hashes.len() == 1 && servers.is_empty() {
        "Authorize Blossom upload".to_owned()
    } else {
        format!("Authorize Blossom upload of {} blob(s)", hashes.len())
    };
    let builder = EventBuilder::new(Kind::BlossomAuth, content).tags(tags);
    let event = signer
        .sign_event_builder_with_description(builder, "Blossom upload authorization")
        .await
        .context("failed to sign the Blossom upload authorization")?;
    Ok((event, expires))
}

fn authorization_header(event: &Event) -> Result<HeaderValue> {
    let event = serde_json::to_vec(event).context("failed to encode Blossom authorization")?;
    // Padded standard Base64 is accepted by Base64url-capable reference
    // servers and by deployed servers which have not yet adopted BUD-11's
    // newer Base64url wording (notably blossom.primal.net).
    HeaderValue::from_str(&format!("Nostr {}", STANDARD.encode(event)))
        .context("failed to construct the Blossom Authorization header")
}

#[derive(Clone, Debug)]
struct CompatibleAuthorization {
    bud11: HeaderValue,
    legacy: HeaderValue,
    encoding_preference: AuthorizationEncodingPreference,
}

fn compatible_authorization_headers(
    event: &Event,
    encoding_preference: AuthorizationEncodingPreference,
) -> Result<CompatibleAuthorization> {
    let event = serde_json::to_vec(event).context("failed to encode Blossom authorization")?;
    let header = |encoded: String| {
        HeaderValue::from_str(&format!("Nostr {encoded}"))
            .context("failed to construct the Blossom Authorization header")
    };
    Ok(CompatibleAuthorization {
        bud11: header(URL_SAFE_NO_PAD.encode(&event))?,
        legacy: header(STANDARD.encode(event))?,
        encoding_preference,
    })
}

fn blossom_endpoint_url(server_url: &str, endpoint: &str) -> Result<Url> {
    let mut server = Url::parse(server_url).context("invalid Blossom server URL")?;
    if !matches!(server.scheme(), "http" | "https") || server.host_str().is_none() {
        bail!("Blossom server URL must be an absolute HTTP or HTTPS URL");
    }
    if !server.username().is_empty() || server.password().is_some() {
        bail!("Blossom server URL must not contain embedded credentials");
    }
    if !matches!(server.path(), "" | "/") || server.query().is_some() || server.fragment().is_some()
    {
        bail!("Blossom server URL must not contain a path, query, or fragment");
    }
    server.set_path("/");
    server
        .join(endpoint)
        .with_context(|| format!("failed to construct the Blossom {endpoint} URL"))
}

fn validate_descriptor(descriptor: &BlobDescriptor, snapshot: &FileSnapshot) -> Result<()> {
    if descriptor.sha256 != snapshot.sha256 {
        bail!("Blossom descriptor SHA-256 does not match the uploaded bytes");
    }
    if descriptor.size != snapshot.size {
        bail!("Blossom descriptor size does not match the uploaded bytes");
    }
    if !snapshot_media_type_matches(snapshot, &descriptor.mime_type) {
        bail!("Blossom descriptor MIME type does not match the upload");
    }
    validate_blob_url(&descriptor.url, &snapshot.sha256)
}

fn snapshot_media_type_matches(snapshot: &FileSnapshot, actual: &str) -> bool {
    let expected = snapshot.mime_type.as_str();
    if actual.eq_ignore_ascii_case(expected) {
        return true;
    }
    let filename = snapshot.filename.to_ascii_lowercase();
    let expected = expected.to_ascii_lowercase();
    let actual = actual.to_ascii_lowercase();
    match filename.rsplit_once('.').map(|(_, extension)| extension) {
        Some("map") => {
            expected == "application/json"
                && matches!(actual.as_str(), "application/json" | "text/plain")
        }
        Some("js" | "mjs") => {
            matches!(
                expected.as_str(),
                "application/javascript" | "text/javascript"
            ) && matches!(
                actual.as_str(),
                "application/javascript" | "text/javascript"
            )
        }
        Some("webmanifest") => {
            matches!(
                expected.as_str(),
                "application/manifest+json" | "application/json"
            ) && matches!(
                actual.as_str(),
                "application/manifest+json" | "application/json"
            )
        }
        Some("ico") => {
            matches!(
                expected.as_str(),
                "image/vnd.microsoft.icon" | "image/x-icon"
            ) && matches!(actual.as_str(), "image/vnd.microsoft.icon" | "image/x-icon")
        }
        _ => false,
    }
}

fn validate_blob_url(url: &Url, sha256: &str) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("Blossom descriptor URL must be an absolute HTTP or HTTPS URL");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("Blossom descriptor URL must not contain embedded credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("Blossom descriptor URL must not contain a query or fragment");
    }
    let filename = url
        .path_segments()
        .and_then(Iterator::last)
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| anyhow!("Blossom descriptor URL must identify the uploaded hash"))?;
    if filename.split('.').next() != Some(sha256) {
        bail!("Blossom descriptor URL does not identify the uploaded SHA-256");
    }
    Ok(())
}

/// Copy a local regular file into a stable, bounded snapshot without retaining
/// its complete contents in memory.
pub async fn snapshot_local_file(request: LocalFileRequest) -> Result<FileSnapshot> {
    if request.max_bytes == 0 {
        bail!("asset byte limit must be greater than zero");
    }

    let (filename, mut warnings) =
        source_filename(&request.source_path, request.filename.as_deref())?;
    let mime = infer_mime_type(request.mime_type.as_deref(), None, &filename)?;
    warnings.extend(mime.warnings);

    tokio::task::spawn_blocking(move || {
        snapshot_local_file_sync(request, filename, mime.mime_type, warnings)
    })
    .await
    .context("local asset snapshot task failed")?
}

fn snapshot_local_file_sync(
    request: LocalFileRequest,
    filename: String,
    mime_type: String,
    warnings: Vec<DownloadWarning>,
) -> Result<FileSnapshot> {
    let mut source = File::open(&request.source_path).with_context(|| {
        format!(
            "failed to open local asset {}",
            request.source_path.display()
        )
    })?;
    if !source
        .metadata()
        .context("failed to inspect the local asset")?
        .is_file()
    {
        bail!("local asset must be a regular file");
    }

    let mut snapshot = NamedTempFile::new().context("failed to create a stable asset snapshot")?;
    let mut engine = sha256::Hash::engine();
    let mut size = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = source
            .read(&mut buffer)
            .context("failed while reading the local asset")?;
        if read == 0 {
            break;
        }

        let read_u64 = u64::try_from(read)
            .map_err(|_| anyhow!("local asset chunk length does not fit in u64"))?;
        size = size
            .checked_add(read_u64)
            .ok_or_else(|| anyhow!("local asset byte length overflowed u64"))?;
        if size > request.max_bytes {
            bail!(
                "local asset exceeds the configured {} byte limit",
                request.max_bytes
            );
        }

        engine.input(&buffer[..read]);
        snapshot
            .write_all(&buffer[..read])
            .context("failed while writing the stable asset snapshot")?;
    }
    snapshot
        .flush()
        .context("failed to flush the stable asset snapshot")?;

    Ok(FileSnapshot {
        file: snapshot,
        filename,
        mime_type,
        sha256: sha256::Hash::from_engine(engine).to_string(),
        size,
        warnings,
    })
}

fn source_filename(path: &Path, explicit: Option<&str>) -> Result<(String, Vec<DownloadWarning>)> {
    let original = match explicit {
        Some(explicit) => explicit,
        None => path
            .file_name()
            .ok_or_else(|| anyhow!("local asset path has no filename"))?
            .to_str()
            .ok_or_else(|| anyhow!("local asset filename is not valid UTF-8"))?,
    };
    let filename = sanitize_filename(original)
        .ok_or_else(|| anyhow!("local asset filename is empty or unsafe"))?;
    let warnings = if filename == original {
        Vec::new()
    } else {
        vec![DownloadWarning {
            code: DownloadWarningCode::FilenameSanitized,
            message: "the local asset filename was sanitized".to_owned(),
        }]
    };
    Ok((filename, warnings))
}

/// NIP-B7 user Blossom server list.
pub const BLOSSOM_SERVER_LIST_KIND: Kind = Kind::Custom(10_063);

/// The latest server declaration and its canonical, ordered server roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlossomServerList {
    pub event_id: EventId,
    pub created_at: Timestamp,
    pub servers: Vec<Url>,
}

/// Build the query used for both relay discovery and local-cache lookup.
pub fn blossom_server_list_filter(author: PublicKey) -> Filter {
    Filter::new().author(author).kind(BLOSSOM_SERVER_LIST_KIND)
}

/// Resolve and parse the latest matching kind-10063 event.
pub fn blossom_server_list_from_events(
    author: PublicKey,
    events: &[Event],
) -> Result<BlossomServerList> {
    let event = latest_event(
        events
            .iter()
            .filter(|event| event.kind == BLOSSOM_SERVER_LIST_KIND && event.pubkey == author),
    )
    .with_context(|| {
        format!(
            "no Blossom server list (kind 10063) was found for {}",
            author.to_hex()
        )
    })?;

    let mut servers = Vec::new();
    let mut seen = HashSet::new();
    for tag in event.tags.iter() {
        match tag.as_slice() {
            [name, value, ..] if name == "server" => {
                let server = canonicalize_blossom_server_root(value).with_context(|| {
                    format!(
                        "invalid Blossom server in kind-10063 event {}",
                        event.id.to_hex()
                    )
                })?;
                if seen.insert(server.to_string()) {
                    servers.push(server);
                }
            }
            [name, ..] if name == "server" => bail!(
                "invalid server tag in kind-10063 event {}: the server URL is missing",
                event.id.to_hex()
            ),
            _ => {}
        }
    }
    if servers.is_empty() {
        bail!(
            "the latest Blossom server list event {} declares no servers",
            event.id.to_hex()
        );
    }

    Ok(BlossomServerList {
        event_id: event.id,
        created_at: event.created_at,
        servers,
    })
}

/// Validate a Blossom server URL and return a stable root for endpoint joins.
///
/// HTTP is accepted for local and explicitly configured servers. Credentials,
/// query strings, and fragments are rejected. Paths are reduced to `/` because
/// Blossom endpoints are rooted at the server origin.
pub fn canonicalize_blossom_server_root(value: &str) -> Result<Url> {
    if value.is_empty() || value.trim() != value {
        bail!("Blossom server URL must be non-empty and contain no surrounding whitespace");
    }

    let mut url = Url::parse(value)
        .with_context(|| format!("failed to parse Blossom server URL {value:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Blossom server URL {value:?} must use the http or https scheme");
    }
    if url.cannot_be_a_base() || url.host().is_none() {
        bail!("Blossom server URL {value:?} must be an absolute server root");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("Blossom server URL {value:?} must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("Blossom server URL {value:?} must not contain a query or fragment");
    }

    url.set_path("/");
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Result, anyhow, bail};
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use nostr::prelude::{
        Event, EventBuilder, Keys, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
    };
    use tempfile::tempdir;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        task::JoinHandle,
    };

    use super::*;

    const SERVER_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_TEST_REQUEST_BYTES: usize = 1024 * 1024;

    #[derive(Debug)]
    struct CapturedRequest {
        head: String,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct TestResponse {
        status: &'static str,
        headers: Vec<(String, String)>,
        body: String,
    }

    #[derive(Default)]
    struct RecordingBlossomProgress {
        events: Mutex<Vec<BlossomProgressEvent>>,
    }

    impl BlossomProgress for RecordingBlossomProgress {
        fn update(&self, event: &BlossomProgressEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    struct PlacementFinishedSignal {
        server: Url,
        signal: Mutex<Option<oneshot::Sender<()>>>,
    }

    impl BlossomProgress for PlacementFinishedSignal {
        fn update(&self, event: &BlossomProgressEvent) {
            if matches!(
                event,
                BlossomProgressEvent::PlacementFinished {
                    server,
                    status: BlossomServerStatus::Stored,
                    ..
                } if server == &self.server
            ) {
                if let Some(signal) = self.signal.lock().unwrap().take() {
                    let _ = signal.send(());
                }
            }
        }
    }

    async fn spawn_one_shot_server(
        response: impl FnOnce(&str) -> TestResponse,
    ) -> Result<(String, JoinHandle<Result<CapturedRequest>>)> {
        spawn_observed_server(response, || Ok(())).await
    }

    async fn spawn_head_server(
        response: impl FnOnce(&str) -> TestResponse,
    ) -> Result<(String, JoinHandle<Result<CapturedRequest>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let response = response(&base_url);
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom HEAD request")??;
            let request = read_header_only_request(&mut stream).await?;
            let mut wire_response =
                format!("HTTP/1.1 {}\r\nConnection: close\r\n", response.status);
            for (name, value) in response.headers {
                wire_response.push_str(&format!("{name}: {value}\r\n"));
            }
            wire_response.push_str("\r\n");
            tokio::time::timeout(SERVER_TIMEOUT, stream.write_all(wire_response.as_bytes()))
                .await
                .context("timed out writing Blossom HEAD response")??;
            Ok(request)
        });
        Ok((base_url, task))
    }

    async fn spawn_repeated_head_server(
        request_count: usize,
        response: TestResponse,
    ) -> Result<(String, JoinHandle<Result<Vec<CapturedRequest>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                    .await
                    .context("timed out waiting for repeated Blossom HEAD request")??;
                requests.push(read_header_only_request(&mut stream).await?);
                let mut wire_response =
                    format!("HTTP/1.1 {}\r\nConnection: close\r\n", response.status);
                for (name, value) in &response.headers {
                    wire_response.push_str(&format!("{name}: {value}\r\n"));
                }
                wire_response.push_str("\r\n");
                tokio::time::timeout(SERVER_TIMEOUT, stream.write_all(wire_response.as_bytes()))
                    .await
                    .context("timed out writing repeated Blossom HEAD response")??;
            }
            Ok(requests)
        });
        Ok((base_url, task))
    }

    async fn spawn_observed_server(
        response: impl FnOnce(&str) -> TestResponse,
        observe: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<(String, JoinHandle<Result<CapturedRequest>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let response = response(&base_url);
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom request")??;
            let request = read_request(&mut stream).await?;
            observe()?;
            let mut wire_response = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                response.status,
                response.body.len()
            );
            for (name, value) in response.headers {
                wire_response.push_str(&format!("{name}: {value}\r\n"));
            }
            wire_response.push_str("\r\n");
            wire_response.push_str(&response.body);
            tokio::time::timeout(SERVER_TIMEOUT, stream.write_all(wire_response.as_bytes()))
                .await
                .context("timed out writing Blossom response")??;
            Ok(request)
        });
        Ok((base_url, task))
    }

    async fn spawn_dropped_response_server() -> Result<(String, JoinHandle<Result<CapturedRequest>>)>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom request")??;
            let request = read_request(&mut stream).await?;
            drop(stream);
            Ok(request)
        });
        Ok((base_url, task))
    }

    async fn spawn_presence_then_upload_server(
        sha256: String,
        size: u64,
        mime_type: String,
    ) -> Result<(
        String,
        JoinHandle<Result<(CapturedRequest, CapturedRequest, CapturedRequest)>>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let response_base = base_url.clone();
        let task = tokio::spawn(async move {
            let (mut head_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom presence check")??;
            let head = read_header_only_request(&mut head_stream).await?;
            tokio::time::timeout(
                SERVER_TIMEOUT,
                head_stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ),
            )
            .await
            .context("timed out writing Blossom presence response")??;

            let (mut upload_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom upload")??;
            let upload = read_request(&mut upload_stream).await?;
            let body = descriptor_json(&response_base, &sha256, size, &mime_type);
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            tokio::time::timeout(SERVER_TIMEOUT, upload_stream.write_all(response.as_bytes()))
                .await
                .context("timed out writing Blossom upload response")??;

            let (mut verify_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom post-upload verification")??;
            let verify = read_header_only_request(&mut verify_stream).await?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: {mime_type}\r\nConnection: close\r\n\r\n"
            );
            verify_stream.write_all(response.as_bytes()).await?;
            Ok((head, upload, verify))
        });
        Ok((base_url, task))
    }

    async fn spawn_gated_presence_then_upload_server(
        sha256: String,
        size: u64,
        mime_type: String,
    ) -> Result<(
        String,
        JoinHandle<Result<(CapturedRequest, CapturedRequest, CapturedRequest)>>,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let response_base = base_url.clone();
        let (verification_started_tx, verification_started_rx) = oneshot::channel();
        let (release_verification_tx, release_verification_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut head_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for gated Blossom presence check")??;
            let head = read_header_only_request(&mut head_stream).await?;
            head_stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;

            let (mut upload_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for gated Blossom upload")??;
            let upload = read_request(&mut upload_stream).await?;
            let body = descriptor_json(&response_base, &sha256, size, &mime_type);
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            upload_stream.write_all(response.as_bytes()).await?;

            let (mut verify_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for gated Blossom verification")??;
            let verify = read_header_only_request(&mut verify_stream).await?;
            let _ = verification_started_tx.send(());
            let _ = release_verification_rx.await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: {mime_type}\r\nConnection: close\r\n\r\n"
            );
            verify_stream.write_all(response.as_bytes()).await?;
            Ok((head, upload, verify))
        });
        Ok((
            base_url,
            task,
            verification_started_rx,
            release_verification_tx,
        ))
    }

    async fn spawn_authorization_fallback_server(
        sha256: String,
        size: u64,
        mime_type: String,
    ) -> Result<(
        String,
        JoinHandle<
            Result<(
                CapturedRequest,
                CapturedRequest,
                CapturedRequest,
                CapturedRequest,
            )>,
        >,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let response_base = base_url.clone();
        let task = tokio::spawn(async move {
            let (mut head_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom presence check")??;
            let head = read_header_only_request(&mut head_stream).await?;
            head_stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;

            let (mut primary_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for BUD-11 Blossom upload")??;
            let primary = read_request(&mut primary_stream).await?;
            primary_stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: 29\r\nConnection: close\r\n\r\ninvalid base64 for auth event",
                )
                .await?;

            let (mut legacy_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for legacy Blossom upload")??;
            let legacy = read_request(&mut legacy_stream).await?;
            let body = descriptor_json(&response_base, &sha256, size, &mime_type);
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            legacy_stream.write_all(response.as_bytes()).await?;

            let (mut verify_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom post-upload verification")??;
            let verify = read_header_only_request(&mut verify_stream).await?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: {mime_type}\r\nConnection: close\r\n\r\n"
            );
            verify_stream.write_all(response.as_bytes()).await?;
            Ok((head, primary, legacy, verify))
        });
        Ok((base_url, task))
    }

    async fn spawn_uncertain_upload_server(
        size: u64,
        mime_type: String,
    ) -> Result<(
        String,
        JoinHandle<Result<(CapturedRequest, CapturedRequest, CapturedRequest)>>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let (mut preflight_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom presence check")??;
            let preflight = read_header_only_request(&mut preflight_stream).await?;
            preflight_stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;

            let (mut upload_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom upload")??;
            let upload = read_request(&mut upload_stream).await?;
            drop(upload_stream);

            let (mut verify_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom recovery verification")??;
            let verify = read_header_only_request(&mut verify_stream).await?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: {mime_type}\r\nConnection: close\r\n\r\n"
            );
            verify_stream.write_all(response.as_bytes()).await?;
            Ok((preflight, upload, verify))
        });
        Ok((base_url, task))
    }

    async fn spawn_transient_presence_server(
        sha256: String,
        size: u64,
        mime_type: String,
    ) -> Result<(
        String,
        JoinHandle<
            Result<(
                CapturedRequest,
                CapturedRequest,
                CapturedRequest,
                CapturedRequest,
            )>,
        >,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let response_base = base_url.clone();
        let task = tokio::spawn(async move {
            let (mut transient_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for transient Blossom presence check")??;
            let transient = read_header_only_request(&mut transient_stream).await?;
            transient_stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;

            let (mut missing_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for retried Blossom presence check")??;
            let missing = read_header_only_request(&mut missing_stream).await?;
            missing_stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;

            let (mut upload_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom upload")??;
            let upload = read_request(&mut upload_stream).await?;
            let body = descriptor_json(&response_base, &sha256, size, &mime_type);
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            upload_stream.write_all(response.as_bytes()).await?;

            let (mut verify_stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom post-upload verification")??;
            let verify = read_header_only_request(&mut verify_stream).await?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: {mime_type}\r\nConnection: close\r\n\r\n"
            );
            verify_stream.write_all(response.as_bytes()).await?;
            Ok((transient, missing, upload, verify))
        });
        Ok((base_url, task))
    }

    async fn read_request(stream: &mut TcpStream) -> Result<CapturedRequest> {
        let mut bytes = Vec::new();
        let header_end = loop {
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
            if bytes.len() >= MAX_TEST_REQUEST_BYTES {
                bail!("test request headers exceeded limit");
            }
            let mut chunk = [0_u8; 8192];
            let read = tokio::time::timeout(SERVER_TIMEOUT, stream.read(&mut chunk))
                .await
                .context("timed out reading Blossom request headers")??;
            if read == 0 {
                bail!("connection closed before request headers completed");
            }
            bytes.extend_from_slice(&chunk[..read]);
        };

        let head = String::from_utf8(bytes[..header_end].to_vec())?;
        let content_length = request_header(&head, "content-length")
            .context("upload omitted Content-Length")?
            .parse::<usize>()?;
        if header_end
            .checked_add(content_length)
            .is_none_or(|length| length > MAX_TEST_REQUEST_BYTES)
        {
            bail!("test request body exceeded limit");
        }
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 8192];
            let read = tokio::time::timeout(SERVER_TIMEOUT, stream.read(&mut chunk))
                .await
                .context("timed out reading Blossom request body")??;
            if read == 0 {
                bail!("connection closed before request body completed");
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let body = bytes[header_end..header_end + content_length].to_vec();
        Ok(CapturedRequest { head, body })
    }

    async fn read_header_only_request(stream: &mut TcpStream) -> Result<CapturedRequest> {
        let mut bytes = Vec::new();
        loop {
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                return Ok(CapturedRequest {
                    head: String::from_utf8(bytes[..offset + 4].to_vec())?,
                    body: Vec::new(),
                });
            }
            if bytes.len() >= MAX_TEST_REQUEST_BYTES {
                bail!("test request headers exceeded limit");
            }
            let mut chunk = [0_u8; 8192];
            let read = tokio::time::timeout(SERVER_TIMEOUT, stream.read(&mut chunk))
                .await
                .context("timed out reading Blossom request headers")??;
            if read == 0 {
                bail!("connection closed before request headers completed");
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
    }

    fn request_header<'a>(head: &'a str, wanted: &str) -> Option<&'a str> {
        head.lines().skip(1).find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted).then(|| value.trim())
        })
    }

    async fn completed_request(
        mut task: JoinHandle<Result<CapturedRequest>>,
    ) -> Result<CapturedRequest> {
        match tokio::time::timeout(SERVER_TIMEOUT, &mut task).await {
            Ok(result) => result.context("Blossom test server task failed")?,
            Err(_) => {
                task.abort();
                Err(anyhow!("timed out waiting for Blossom test server"))
            }
        }
    }

    fn descriptor_json(base_url: &str, sha256: &str, size: u64, mime_type: &str) -> String {
        serde_json::json!({
            "url": format!("{base_url}/{sha256}.apk"),
            "sha256": sha256,
            "size": size,
            "type": mime_type,
            "uploaded": 1,
        })
        .to_string()
    }

    fn event_tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
        event.tags.iter().find_map(|tag| {
            let values = tag.as_slice();
            (values.first().map(String::as_str) == Some(name))
                .then(|| values.get(1).map(String::as_str))
                .flatten()
        })
    }

    #[test]
    fn authorization_headers_cover_bud11_and_legacy_base64() -> Result<()> {
        let keys = Keys::generate();
        let event = server_list_event(&keys, 1, "compatibility", []);
        let event_json = serde_json::to_vec(&event)?;
        let headers =
            compatible_authorization_headers(&event, AuthorizationEncodingPreference::Bud11)?;
        let bud11 = headers
            .bud11
            .to_str()?
            .strip_prefix("Nostr ")
            .context("authorization header omitted the Nostr scheme")?;
        let legacy = headers
            .legacy
            .to_str()?
            .strip_prefix("Nostr ")
            .context("authorization header omitted the Nostr scheme")?;

        assert_eq!(bud11, URL_SAFE_NO_PAD.encode(&event_json));
        assert_eq!(legacy, STANDARD.encode(&event_json));
        assert_eq!(URL_SAFE_NO_PAD.decode(bud11)?, event_json);
        assert_eq!(STANDARD.decode(legacy)?, event_json);
        assert_eq!(
            authorization_header(&event)?,
            headers.legacy,
            "the low-level upload primitive retains its legacy encoding"
        );
        Ok(())
    }

    #[tokio::test]
    async fn batch_authorization_scopes_all_hashes_and_server_domains() -> Result<()> {
        let signer = NgitSigner::Keys(Keys::generate());
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let servers = [
            Url::parse("https://BLOSSOM.example:443")?,
            Url::parse("http://localhost:3000")?,
        ];

        let event =
            upload_authorization(&[first.as_str(), second.as_str()], &servers, &signer).await?;
        let tags = event.tags.iter().map(Tag::as_slice).collect::<Vec<_>>();

        assert!(tags.iter().any(|tag| tag == &["t", "upload"]));
        assert!(tags.iter().any(|tag| tag == &["x", first.as_str()]));
        assert!(tags.iter().any(|tag| tag == &["x", second.as_str()]));
        assert!(tags.iter().any(|tag| tag == &["server", "blossom.example"]));
        assert!(tags.iter().any(|tag| tag == &["server", "localhost"]));
        assert_eq!(event.content, "Authorize Blossom upload of 2 blob(s)");
        Ok(())
    }

    #[tokio::test]
    async fn batch_upload_checks_presence_then_reuses_authorization_for_put() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_presence_then_upload_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let server_url = Url::parse(&server_url)?;
        let signer = NgitSigner::Keys(Keys::generate());

        let result = upload_snapshot_batch_to_servers(
            std::slice::from_ref(&server_url),
            &[&snapshot],
            &signer,
            2,
        )
        .await?;
        let (head, upload, verify) = tokio::time::timeout(SERVER_TIMEOUT, server)
            .await
            .context("timed out waiting for batch upload server")???;

        assert!(
            head.head
                .starts_with(&format!("HEAD /{} HTTP/1.1\r\n", snapshot.sha256))
        );
        assert!(upload.head.starts_with("PUT /upload HTTP/1.1\r\n"));
        assert!(
            verify
                .head
                .starts_with(&format!("HEAD /{} HTTP/1.1\r\n", snapshot.sha256))
        );
        assert_eq!(upload.body, b"static site");
        assert!(request_header(&upload.head, "authorization").is_some());
        assert_eq!(result.blobs.len(), 1);
        assert_eq!(
            result.blobs[0].servers[0].status,
            BlossomServerStatus::Stored
        );
        Ok(())
    }

    #[tokio::test]
    async fn batch_progress_finishes_each_placement_without_waiting_for_slow_peers() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"independent placement completion")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (fast_url, fast_server) = spawn_presence_then_upload_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let (slow_url, slow_server, slow_verification_started, release_slow_verification) =
            spawn_gated_presence_then_upload_server(
                snapshot.sha256.clone(),
                snapshot.size,
                snapshot.mime_type.clone(),
            )
            .await?;
        let fast_url = Url::parse(&fast_url)?;
        let slow_url = Url::parse(&slow_url)?;
        let (fast_finished_tx, fast_finished_rx) = oneshot::channel();
        let progress = Arc::new(PlacementFinishedSignal {
            server: fast_url.clone(),
            signal: Mutex::new(Some(fast_finished_tx)),
        });
        let servers = [fast_url, slow_url];
        let signer = NgitSigner::Keys(Keys::generate());
        let snapshots = [&snapshot];
        let upload = upload_snapshot_batch_to_servers_with_progress(
            &servers, &snapshots, &signer, 2, progress,
        );
        let observe_independent_completion = async move {
            slow_verification_started
                .await
                .context("slow Blossom placement never reached verification")?;
            fast_finished_rx
                .await
                .context("fast Blossom placement stayed active behind its slow peer")?;
            release_slow_verification
                .send(())
                .map_err(|_| anyhow!("slow Blossom placement stopped before release"))?;
            Ok::<(), anyhow::Error>(())
        };

        let (result, observation) = tokio::time::timeout(SERVER_TIMEOUT, async {
            tokio::join!(upload, observe_independent_completion)
        })
        .await
        .context("fast Blossom placement did not finish independently")?;
        observation?;
        let result = result?;
        tokio::time::timeout(SERVER_TIMEOUT, fast_server)
            .await
            .context("timed out waiting for fast Blossom server")???;
        tokio::time::timeout(SERVER_TIMEOUT, slow_server)
            .await
            .context("timed out waiting for slow Blossom server")???;
        assert!(
            result.blobs[0]
                .servers
                .iter()
                .all(|outcome| outcome.status == BlossomServerStatus::Stored)
        );
        Ok(())
    }

    #[tokio::test]
    async fn release_batch_succeeds_when_each_blob_has_one_confirmed_server() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"resilient release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (failed_url, failed_server) = spawn_head_server(|_| TestResponse {
            status: "403 Forbidden",
            headers: Vec::new(),
            body: String::new(),
        })
        .await?;
        let (confirmed_url, confirmed_server) = spawn_presence_then_upload_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let servers = [Url::parse(&failed_url)?, Url::parse(&confirmed_url)?];

        let result = upload_release_snapshot_batch_to_servers_with_progress(
            &servers,
            &[&snapshot],
            &NgitSigner::Keys(Keys::generate()),
            2,
            Arc::new(HiddenBlossomProgress),
        )
        .await?;
        completed_request(failed_server).await?;
        tokio::time::timeout(SERVER_TIMEOUT, confirmed_server)
            .await
            .context("timed out waiting for confirmed release placement")???;

        assert_eq!(
            result.blobs[0]
                .servers
                .iter()
                .map(|outcome| outcome.status)
                .collect::<Vec<_>>(),
            [BlossomServerStatus::Failed, BlossomServerStatus::Stored]
        );
        let upload = multi_server_upload_from_batch_outcome(&snapshot, &result.blobs[0])?;
        assert!(
            upload.primary.url.as_str().starts_with(&confirmed_url),
            "the first confirmed server must supply the published URL"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resilient_batch_keeps_bud11_batching_when_one_server_fails() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"resilient static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (failed_url, failed_server) = spawn_head_server(|_| TestResponse {
            status: "403 Forbidden",
            headers: Vec::new(),
            body: String::new(),
        })
        .await?;
        let (confirmed_url, confirmed_server) = spawn_presence_then_upload_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let servers = [Url::parse(&failed_url)?, Url::parse(&confirmed_url)?];

        let result = upload_resilient_snapshot_batch_to_servers_with_progress(
            &servers,
            &[&snapshot],
            &NgitSigner::Keys(Keys::generate()),
            2,
            Arc::new(HiddenBlossomProgress),
        )
        .await?;
        completed_request(failed_server).await?;
        tokio::time::timeout(SERVER_TIMEOUT, confirmed_server)
            .await
            .context("timed out waiting for confirmed resilient placement")???;

        assert_eq!(
            result.blobs[0]
                .servers
                .iter()
                .map(|outcome| outcome.status)
                .collect::<Vec<_>>(),
            [BlossomServerStatus::Failed, BlossomServerStatus::Stored]
        );
        Ok(())
    }

    #[tokio::test]
    async fn resilient_batch_stops_probing_a_transiently_failed_server() -> Result<()> {
        let directory = tempdir()?;
        let mut snapshots = Vec::new();
        for index in 0_u8..10 {
            let path = directory.path().join(format!("blob-{index}.bin"));
            std::fs::write(&path, [index; 16])?;
            snapshots.push(snapshot_local_file(LocalFileRequest::new(path)).await?);
        }
        let snapshot_refs = snapshots.iter().collect::<Vec<_>>();
        let (failed_url, failed_server) = spawn_repeated_head_server(
            PRESENCE_CIRCUIT_FAILURE_THRESHOLD,
            TestResponse {
                status: "503 Service Unavailable",
                headers: Vec::new(),
                body: String::new(),
            },
        )
        .await?;
        let (confirmed_url, confirmed_server) = spawn_repeated_head_server(
            snapshots.len(),
            TestResponse {
                status: "200 OK",
                headers: vec![
                    ("Content-Length".to_owned(), snapshots[0].size.to_string()),
                    ("Content-Type".to_owned(), snapshots[0].mime_type.clone()),
                ],
                body: String::new(),
            },
        )
        .await?;
        let servers = [Url::parse(&failed_url)?, Url::parse(&confirmed_url)?];

        let result = upload_resilient_snapshot_batch_to_servers_with_progress(
            &servers,
            &snapshot_refs,
            &NgitSigner::Keys(Keys::generate()),
            1,
            Arc::new(HiddenBlossomProgress),
        )
        .await?;
        let failed_requests = tokio::time::timeout(SERVER_TIMEOUT, failed_server)
            .await
            .context("timed out waiting for failed Blossom server")???;
        let confirmed_requests = tokio::time::timeout(SERVER_TIMEOUT, confirmed_server)
            .await
            .context("timed out waiting for healthy Blossom server")???;

        assert_eq!(
            failed_requests.len(),
            PRESENCE_CIRCUIT_FAILURE_THRESHOLD,
            "the circuit should bound requests to a persistently failing server"
        );
        assert_eq!(confirmed_requests.len(), snapshots.len());
        assert_eq!(
            result
                .blobs
                .iter()
                .filter(|blob| {
                    matches!(
                        blob.servers[0].status,
                        BlossomServerStatus::Failed | BlossomServerStatus::Unknown
                    )
                })
                .count(),
            1
        );
        assert!(
            result.blobs[1..]
                .iter()
                .all(|blob| blob.servers[0].status == BlossomServerStatus::NotAttempted)
        );
        assert!(
            result
                .blobs
                .iter()
                .all(|blob| blob.servers[1].status == BlossomServerStatus::AlreadyPresent)
        );
        Ok(())
    }

    #[tokio::test]
    async fn release_batch_fails_when_a_blob_has_no_confirmed_server() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"unavailable release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (failed_url, failed_server) = spawn_head_server(|_| TestResponse {
            status: "403 Forbidden",
            headers: Vec::new(),
            body: String::new(),
        })
        .await?;

        let error = upload_release_snapshot_batch_to_servers_with_progress(
            &[Url::parse(&failed_url)?],
            &[&snapshot],
            &NgitSigner::Keys(Keys::generate()),
            1,
            Arc::new(HiddenBlossomProgress),
        )
        .await
        .unwrap_err();
        completed_request(failed_server).await?;

        assert!(
            error
                .message
                .contains("1/1 Blossom blobs were not confirmed on any selected server")
        );
        assert!(error.possible_orphan_blobs.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn single_snapshot_placement_uses_the_shared_verified_path() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release or container")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_presence_then_upload_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let server_url = Url::parse(&server_url)?;

        let result = confirm_snapshot_on_servers(
            std::slice::from_ref(&server_url),
            &snapshot,
            &NgitSigner::Keys(Keys::generate()),
        )
        .await?;
        let (head, upload, verify) = tokio::time::timeout(SERVER_TIMEOUT, server)
            .await
            .context("timed out waiting for single-snapshot placement server")???;

        assert!(head.head.starts_with("HEAD /"));
        assert!(upload.head.starts_with("PUT /upload HTTP/1.1\r\n"));
        assert!(verify.head.starts_with("HEAD /"));
        assert_eq!(result.primary.sha256, snapshot.sha256);
        assert_eq!(result.primary.size, snapshot.size);
        assert_eq!(result.servers.len(), 1);
        assert_eq!(result.servers[0].operation, BlossomServerOperation::Upload);
        assert_eq!(result.servers[0].status, BlossomServerStatus::Stored);
        Ok(())
    }

    #[tokio::test]
    async fn single_snapshot_placement_synthesizes_a_url_for_existing_content() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"already stored")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let expected_size = snapshot.size.to_string();
        let expected_mime = snapshot.mime_type.clone();
        let (server_url, server) = spawn_head_server(move |_| TestResponse {
            status: "200 OK",
            headers: vec![
                ("Content-Length".to_owned(), expected_size),
                ("Content-Type".to_owned(), expected_mime),
            ],
            body: String::new(),
        })
        .await?;
        let server_url = Url::parse(&server_url)?;

        let result = confirm_snapshot_on_servers(
            std::slice::from_ref(&server_url),
            &snapshot,
            &NgitSigner::Keys(Keys::generate()),
        )
        .await?;
        completed_request(server).await?;

        assert_eq!(
            result.primary.url,
            server_url.join(snapshot.sha256.as_str())?
        );
        assert_eq!(result.primary.uploaded, 0);
        assert_eq!(
            result.servers[0].status,
            BlossomServerStatus::AlreadyPresent
        );
        assert!(result.servers[0].descriptor.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn batch_upload_reuses_one_event_for_legacy_authorization_fallback() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_authorization_fallback_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let server_url = Url::parse(&server_url)?;
        let signer = NgitSigner::Keys(Keys::generate());

        let result = upload_snapshot_batch_to_servers(
            std::slice::from_ref(&server_url),
            &[&snapshot],
            &signer,
            1,
        )
        .await?;
        let (_, primary, legacy, _) = tokio::time::timeout(SERVER_TIMEOUT, server)
            .await
            .context("timed out waiting for authorization fallback server")???;
        let encoded = |request: &CapturedRequest| -> Result<String> {
            Ok(request_header(&request.head, "authorization")
                .context("upload omitted authorization")?
                .strip_prefix("Nostr ")
                .context("authorization header omitted the Nostr scheme")?
                .to_owned())
        };
        let primary_json = URL_SAFE_NO_PAD.decode(encoded(&primary)?)?;
        let legacy_json = STANDARD.decode(encoded(&legacy)?)?;

        assert_eq!(primary_json, legacy_json);
        assert_eq!(
            result.blobs[0].servers[0].status,
            BlossomServerStatus::Stored
        );
        Ok(())
    }

    #[test]
    fn authorization_encoding_fallback_covers_deployed_rejection_statuses() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
        ] {
            let error = BlobRequestError::http(
                anyhow!("authorization rejected"),
                status,
                RequestFailureKind::Definite,
                false,
            );
            assert!(auth_encoding_may_be_rejected(&error));
        }

        let error = BlobRequestError::http(
            anyhow!("upload too large"),
            StatusCode::PAYLOAD_TOO_LARGE,
            RequestFailureKind::Definite,
            false,
        );
        assert!(!auth_encoding_may_be_rejected(&error));
    }

    #[tokio::test]
    async fn batch_progress_counts_compatible_upload_fallback_bytes() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"progress accounting")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_authorization_fallback_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let server_url = Url::parse(&server_url)?;
        let progress = Arc::new(RecordingBlossomProgress::default());

        upload_snapshot_batch_to_servers_with_progress(
            std::slice::from_ref(&server_url),
            &[&snapshot],
            &NgitSigner::Keys(Keys::generate()),
            1,
            progress.clone(),
        )
        .await?;
        tokio::time::timeout(SERVER_TIMEOUT, server)
            .await
            .context("timed out waiting for authorization fallback server")???;

        let events = progress.events.lock().unwrap();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                BlossomProgressEvent::PresenceCheckFinished {
                    checked: 1,
                    checks: 1,
                    confirmed: 0,
                    missing: 1,
                    unavailable: 0,
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                BlossomProgressEvent::UploadBatchStarted {
                    batch: 1,
                    batches: 1,
                    blobs: 1,
                    placements: 1,
                    bytes,
                    ..
                } if *bytes == snapshot.size
            )
        }));
        let request_bytes = events
            .iter()
            .filter_map(|event| match event {
                BlossomProgressEvent::UploadRequestStarted {
                    additional_bytes, ..
                } => Some(*additional_bytes),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(request_bytes, [0, snapshot.size]);
        let uploaded_bytes = events
            .iter()
            .filter_map(|event| match event {
                BlossomProgressEvent::UploadedBytes { bytes, .. } => Some(*bytes),
                _ => None,
            })
            .sum::<u64>();
        assert_eq!(uploaded_bytes, snapshot.size * 2);
        let completed_bodies = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    BlossomProgressEvent::UploadBodyFinished {
                        filename,
                        server,
                        ..
                    } if filename == &snapshot.filename && server == &server_url
                )
            })
            .count();
        assert_eq!(completed_bodies, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                BlossomProgressEvent::VerificationStarted {
                    attempt: 1,
                    max_attempts: PLACEMENT_MAX_ATTEMPTS,
                    timeout_secs: 15,
                    ..
                }
            )
        }));
        assert!(matches!(
            events.last(),
            Some(BlossomProgressEvent::UploadBatchFinished {
                batch: 1,
                batches: 1,
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn batch_upload_recovers_an_uncertain_put_with_strict_presence() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) =
            spawn_uncertain_upload_server(snapshot.size, snapshot.mime_type.clone()).await?;
        let server_url = Url::parse(&server_url)?;
        let signer = NgitSigner::Keys(Keys::generate());

        let result = upload_snapshot_batch_to_servers(
            std::slice::from_ref(&server_url),
            &[&snapshot],
            &signer,
            1,
        )
        .await?;
        let (_, upload, verify) = tokio::time::timeout(SERVER_TIMEOUT, server)
            .await
            .context("timed out waiting for uncertain upload server")???;

        assert!(upload.head.starts_with("PUT /upload HTTP/1.1\r\n"));
        assert!(
            verify
                .head
                .starts_with(&format!("HEAD /{} HTTP/1.1\r\n", snapshot.sha256))
        );
        assert_eq!(
            result.blobs[0].servers[0].status,
            BlossomServerStatus::Stored
        );
        assert!(result.blobs[0].servers[0].descriptor.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn batch_upload_retries_a_transient_presence_response() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_transient_presence_server(
            snapshot.sha256.clone(),
            snapshot.size,
            snapshot.mime_type.clone(),
        )
        .await?;
        let server_url = Url::parse(&server_url)?;
        let signer = NgitSigner::Keys(Keys::generate());

        let result = upload_snapshot_batch_to_servers(
            std::slice::from_ref(&server_url),
            &[&snapshot],
            &signer,
            1,
        )
        .await?;
        let (transient, retry, _, _) = tokio::time::timeout(SERVER_TIMEOUT, server)
            .await
            .context("timed out waiting for transient presence server")???;

        assert!(transient.head.starts_with("HEAD /"));
        assert!(retry.head.starts_with("HEAD /"));
        assert_eq!(
            result.blobs[0].servers[0].status,
            BlossomServerStatus::Stored
        );
        Ok(())
    }

    #[tokio::test]
    async fn presence_requires_matching_length_and_mime_metadata() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let client = blossom_http_client()?;

        let expected_size = snapshot.size.to_string();
        let expected_mime = snapshot.mime_type.clone();
        let (server_url, server) = spawn_head_server(move |_| TestResponse {
            status: "200 OK",
            headers: vec![
                ("Content-Length".to_owned(), expected_size),
                (
                    "Content-Type".to_owned(),
                    format!("{expected_mime}; charset=utf-8"),
                ),
            ],
            body: String::new(),
        })
        .await?;
        assert!(snapshot_is_present(&client, &Url::parse(&server_url)?, &snapshot).await?);
        completed_request(server).await?;

        for (length, mime, expected_message) in [
            (
                snapshot.size.saturating_add(1).to_string(),
                snapshot.mime_type.clone(),
                "does not match the",
            ),
            (
                snapshot.size.to_string(),
                "text/css".to_owned(),
                "does not match snapshot MIME type",
            ),
        ] {
            let (server_url, server) = spawn_head_server(move |_| TestResponse {
                status: "200 OK",
                headers: vec![
                    ("Content-Length".to_owned(), length),
                    ("Content-Type".to_owned(), mime),
                ],
                body: String::new(),
            })
            .await?;
            let error = snapshot_is_present(&client, &Url::parse(&server_url)?, &snapshot)
                .await
                .unwrap_err();
            assert!(error.message.contains(expected_message));
            completed_request(server).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn static_asset_mime_aliases_are_filename_scoped() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static asset")?;
        let mut snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;

        for (filename, expected, actual) in [
            ("app.js.map", "application/json", "text/plain"),
            ("app.js", "text/javascript", "application/javascript"),
            (
                "manifest.webmanifest",
                "application/manifest+json",
                "application/json",
            ),
            ("favicon.ico", "image/vnd.microsoft.icon", "image/x-icon"),
        ] {
            snapshot.filename = filename.to_owned();
            snapshot.mime_type = expected.to_owned();
            assert!(snapshot_media_type_matches(&snapshot, actual));
        }

        snapshot.filename = "site.css".to_owned();
        snapshot.mime_type = "text/css".to_owned();
        assert!(!snapshot_media_type_matches(&snapshot, "text/plain"));
        snapshot.filename = "ngit.bin".to_owned();
        snapshot.mime_type = "application/octet-stream".to_owned();
        assert!(!snapshot_media_type_matches(
            &snapshot,
            "application/x-pie-executable"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn presence_follows_hash_preserving_bud01_redirects() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let expected_size = snapshot.size.to_string();
        let expected_mime = snapshot.mime_type.clone();
        let (destination_url, destination) = spawn_head_server(move |_| TestResponse {
            status: "200 OK",
            headers: vec![
                ("Content-Length".to_owned(), expected_size),
                ("Content-Type".to_owned(), expected_mime),
            ],
            body: String::new(),
        })
        .await?;
        let location = format!("{destination_url}/{}.bin", snapshot.sha256);
        let (server_url, server) = spawn_head_server(move |_| TestResponse {
            status: "307 Temporary Redirect",
            headers: vec![("Location".to_owned(), location)],
            body: String::new(),
        })
        .await?;

        let present = snapshot_is_present(
            &blossom_http_client()?,
            &Url::parse(&server_url)?,
            &snapshot,
        )
        .await?;

        assert!(present);
        completed_request(server).await?;
        completed_request(destination).await?;
        Ok(())
    }

    #[tokio::test]
    async fn presence_rejects_redirects_without_the_requested_hash() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"static site")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_head_server(|base_url| TestResponse {
            status: "308 Permanent Redirect",
            headers: vec![("Location".to_owned(), format!("{base_url}/different.bin"))],
            body: String::new(),
        })
        .await?;

        let error = snapshot_is_present(
            &blossom_http_client()?,
            &Url::parse(&server_url)?,
            &snapshot,
        )
        .await
        .unwrap_err();

        assert!(
            error
                .message
                .contains("does not contain the requested SHA-256")
        );
        completed_request(server).await?;
        Ok(())
    }

    #[test]
    fn batch_authorization_window_covers_every_bounded_upload_wave() -> Result<()> {
        let attempts = PLACEMENT_MAX_ATTEMPTS as u32;
        let operation_window =
            UPLOAD_REQUEST_TIMEOUT * attempts + PRESENCE_REQUEST_TIMEOUT * (attempts * attempts);
        assert_eq!(batch_upload_window(1, 4)?, operation_window);
        assert_eq!(batch_upload_window(8, 4)?, operation_window * 2);
        assert_eq!(batch_upload_window(9, 4)?, operation_window * 3);
        Ok(())
    }

    #[tokio::test]
    async fn steadily_progressing_upload_can_outlive_the_idle_timeout() -> Result<()> {
        let (server_url, server) = spawn_one_shot_server(|_| TestResponse {
            status: "200 OK",
            headers: Vec::new(),
            body: String::new(),
        })
        .await?;
        let (activity_tx, activity_rx) = tokio::sync::mpsc::unbounded_channel();
        let chunk_count = 5_u64;
        let body = stream::unfold((0_u64, activity_tx), move |(chunk, activity)| async move {
            if chunk == chunk_count {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            let sent = chunk + 1;
            let _ = activity.send(sent);
            Some((Ok::<Vec<u8>, std::io::Error>(vec![b'x']), (sent, activity)))
        });
        let request = blossom_streaming_http_client()?
            .put(format!("{server_url}/upload"))
            .header(CONTENT_LENGTH, chunk_count)
            .body(reqwest::Body::wrap_stream(body));

        send_upload_request_with_progress_timeout(
            request,
            activity_rx,
            tokio::time::Instant::now() + Duration::from_secs(2),
            Duration::from_millis(80),
            chunk_count,
        )
        .await?;

        let request = completed_request(server).await?;
        assert_eq!(request.body, vec![b'x'; chunk_count as usize]);
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_retains_exact_bytes_after_source_changes() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("ngit.zip");
        let original = vec![0x5a; COPY_BUFFER_BYTES * 2 + 17];
        std::fs::write(&path, &original)?;

        let snapshot = snapshot_local_file(LocalFileRequest::new(&path)).await?;
        std::fs::write(&path, b"changed after snapshot")?;

        let mut retained = Vec::new();
        snapshot.reopen()?.read_to_end(&mut retained)?;
        assert_eq!(retained, original);
        assert_eq!(snapshot.filename, "ngit.zip");
        assert_eq!(snapshot.mime_type, "application/zip");
        assert_eq!(snapshot.size, original.len() as u64);
        assert_eq!(snapshot.sha256, sha256::Hash::hash(&original).to_string());
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_rejects_files_over_the_configured_limit() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"fives")?;
        let mut request = LocalFileRequest::new(file.path());
        request.max_bytes = 4;

        let error = snapshot_local_file(request).await.unwrap_err();
        assert!(format!("{error:#}").contains("4 byte limit"));
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_rejects_non_regular_paths() -> Result<()> {
        let directory = tempdir()?;

        assert!(
            snapshot_local_file(LocalFileRequest::new(directory.path()))
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicit_mime_is_normalized_and_validated() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"asset")?;
        let mut request = LocalFileRequest::new(file.path());
        request.mime_type = Some(" Application/Example; charset=utf-8 ".to_owned());

        let snapshot = snapshot_local_file(request).await?;
        assert_eq!(snapshot.mime_type, "application/example");
        assert_eq!(snapshot.warnings.len(), 1);

        let mut invalid = LocalFileRequest::new(file.path());
        invalid.mime_type = Some("not a mime".to_owned());
        assert!(snapshot_local_file(invalid).await.is_err());
        Ok(())
    }

    fn server_list_event(
        keys: &Keys,
        created_at: u64,
        content: &str,
        tags: impl IntoIterator<Item = Tag>,
    ) -> Event {
        keys.sign_event(
            EventBuilder::new(BLOSSOM_SERVER_LIST_KIND, content)
                .tags(tags)
                .custom_created_at(Timestamp::from_secs(created_at))
                .finalize_unsigned(keys.public_key()),
        )
        .unwrap()
    }

    #[test]
    fn canonicalizes_http_server_roots() {
        assert_eq!(
            canonicalize_blossom_server_root("HTTPS://Example.COM:443/files//")
                .unwrap()
                .as_str(),
            "https://example.com/"
        );
        assert_eq!(
            canonicalize_blossom_server_root("http://localhost:3000")
                .unwrap()
                .as_str(),
            "http://localhost:3000/"
        );
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_server_roots() {
        for value in [
            "ftp://example.com",
            "https://user@example.com",
            "https://example.com?token=secret",
            "https://example.com/#upload",
            " https://example.com",
            "",
        ] {
            assert!(
                canonicalize_blossom_server_root(value).is_err(),
                "accepted {value:?}"
            );
        }
    }

    #[test]
    fn preserves_declaration_order_while_deduplicating_canonical_roots() {
        let keys = Keys::generate();
        let event = server_list_event(
            &keys,
            1,
            "",
            [
                Tag::parse(["server", "HTTPS://Example.COM:443/files", "future-marker"]).unwrap(),
                Tag::parse(["client", "ignored"]).unwrap(),
                Tag::parse(["server", "https://example.com/files/"]).unwrap(),
                Tag::parse(["server", "http://localhost:3000"]).unwrap(),
            ],
        );

        let list = blossom_server_list_from_events(keys.public_key(), &[event]).unwrap();

        assert_eq!(
            list.servers.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://example.com/", "http://localhost:3000/"]
        );
    }

    #[test]
    fn selects_latest_matching_event_using_nip01_ordering() {
        let keys = Keys::generate();
        let other_keys = Keys::generate();
        let old = server_list_event(
            &keys,
            1,
            "old",
            [Tag::parse(["server", "https://old.example"]).unwrap()],
        );
        let a = server_list_event(
            &keys,
            2,
            "a",
            [Tag::parse(["server", "https://a.example"]).unwrap()],
        );
        let b = server_list_event(
            &keys,
            2,
            "b",
            [Tag::parse(["server", "https://b.example"]).unwrap()],
        );
        let other_author = server_list_event(
            &other_keys,
            3,
            "other",
            [Tag::parse(["server", "https://other.example"]).unwrap()],
        );
        let other_kind = keys
            .sign_event(
                EventBuilder::new(Kind::TextNote, "not a list")
                    .tag(Tag::parse(["server", "https://note.example"]).unwrap())
                    .custom_created_at(Timestamp::from_secs(3))
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();
        let expected = if a.id < b.id { &a } else { &b };

        let list = blossom_server_list_from_events(
            keys.public_key(),
            &[old, a.clone(), b.clone(), other_author, other_kind],
        )
        .unwrap();

        assert_eq!(list.event_id, expected.id);
        assert_eq!(
            list.servers[0],
            canonicalize_blossom_server_root(expected.tags[0].content().unwrap()).unwrap()
        );
    }

    #[test]
    fn reports_absent_empty_and_malformed_latest_lists() {
        let keys = Keys::generate();
        let no_event = blossom_server_list_from_events(keys.public_key(), &[])
            .unwrap_err()
            .to_string();
        assert!(no_event.contains("no Blossom server list (kind 10063)"));

        let empty = server_list_event(&keys, 1, "", []);
        let empty_error = blossom_server_list_from_events(keys.public_key(), &[empty])
            .unwrap_err()
            .to_string();
        assert!(empty_error.contains("declares no servers"));

        let malformed = server_list_event(&keys, 2, "", [Tag::parse(["server"]).unwrap()]);
        let malformed_error = blossom_server_list_from_events(keys.public_key(), &[malformed])
            .unwrap_err()
            .to_string();
        assert!(malformed_error.contains("server URL is missing"));
    }

    #[test]
    fn rejects_invalid_url_in_latest_list_instead_of_falling_back() {
        let keys = Keys::generate();
        let valid = server_list_event(
            &keys,
            1,
            "",
            [Tag::parse(["server", "https://valid.example"]).unwrap()],
        );
        let invalid = server_list_event(
            &keys,
            2,
            "",
            [Tag::parse(["server", "file:///tmp/blobs"]).unwrap()],
        );

        let error = blossom_server_list_from_events(keys.public_key(), &[valid, invalid])
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid Blossom server"));
    }

    #[tokio::test]
    async fn snapshot_sanitizes_the_local_basename() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("bad:name?.zip");
        std::fs::write(&path, b"asset")?;

        let snapshot = snapshot_local_file(LocalFileRequest::new(path)).await?;

        assert_eq!(snapshot.filename, "bad_name_.zip");
        assert_eq!(snapshot.warnings.len(), 1);
        assert_eq!(
            snapshot.warnings[0].code,
            DownloadWarningCode::FilenameSanitized
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicit_filename_controls_published_name_and_mime_inference() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("build-output.bin");
        std::fs::write(&path, b"asset")?;
        let mut request = LocalFileRequest::new(path);
        request.filename = Some("ngit-release.zip".to_owned());

        let snapshot = snapshot_local_file(request).await?;

        assert_eq!(snapshot.filename, "ngit-release.zip");
        assert_eq!(snapshot.mime_type, "application/zip");
        Ok(())
    }

    #[tokio::test]
    async fn upload_streams_snapshot_with_scoped_authorization() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("ngit.apk");
        let original = vec![0x82; COPY_BUFFER_BYTES * 2 + 19];
        std::fs::write(&path, &original)?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(&path)).await?;
        std::fs::write(&path, b"source changed after staging")?;

        let signer_keys = Keys::generate();
        let signer = NgitSigner::Keys(signer_keys.clone());
        for status in ["200 OK", "201 Created"] {
            let expected_hash = snapshot.sha256.clone();
            let expected_mime = snapshot.mime_type.clone();
            let expected_size = snapshot.size;
            let (server_url, server) = spawn_one_shot_server(move |base_url| TestResponse {
                status,
                headers: Vec::new(),
                body: descriptor_json(base_url, &expected_hash, expected_size, &expected_mime),
            })
            .await?;

            let upload =
                upload_snapshot_with_timeout(&server_url, &snapshot, &signer, TOTAL_TIMEOUT)
                    .await?;
            let request = completed_request(server).await?;

            assert!(request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
            assert_eq!(request.body, original);
            assert_eq!(
                request_header(&request.head, "content-length"),
                Some(snapshot.size.to_string().as_str())
            );
            assert_eq!(
                request_header(&request.head, "content-type"),
                Some(snapshot.mime_type.as_str())
            );
            assert_eq!(
                request_header(&request.head, "x-sha-256"),
                Some(snapshot.sha256.as_str())
            );

            let authorization = request_header(&request.head, "authorization")
                .context("upload omitted Authorization")?
                .strip_prefix("Nostr ")
                .context("upload authorization used the wrong scheme")?;
            let event: Event = serde_json::from_slice(&STANDARD.decode(authorization)?)?;
            event.verify()?;
            assert_eq!(event.kind, Kind::BlossomAuth);
            assert_eq!(event.pubkey, signer_keys.public_key());
            assert_eq!(event_tag(&event, "t"), Some("upload"));
            assert_eq!(event_tag(&event, "x"), Some(snapshot.sha256.as_str()));
            let expiration = event_tag(&event, "expiration")
                .context("authorization omitted expiration")?
                .parse::<u64>()?;
            let lifetime = expiration.saturating_sub(event.created_at.as_secs());
            assert!((299..=300).contains(&lifetime));

            assert_eq!(upload.descriptor.sha256, snapshot.sha256);
            assert_eq!(upload.descriptor.size, snapshot.size);
            assert_eq!(upload.descriptor.mime_type, snapshot.mime_type);
            assert_eq!(upload.newly_stored, status == "201 Created");
        }
        Ok(())
    }

    #[tokio::test]
    async fn mirror_sends_exact_blob_claim_with_scoped_authorization() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let source = Url::parse(&format!("https://primary.example/{}.apk", snapshot.sha256))?;
        let signer_keys = Keys::generate();
        let signer = NgitSigner::Keys(signer_keys.clone());

        for status in ["200 OK", "201 Created"] {
            let expected_hash = snapshot.sha256.clone();
            let expected_mime = snapshot.mime_type.clone();
            let expected_size = snapshot.size;
            let (server_url, server) = spawn_one_shot_server(move |base_url| TestResponse {
                status,
                headers: Vec::new(),
                body: descriptor_json(base_url, &expected_hash, expected_size, &expected_mime),
            })
            .await?;

            let mirrored = mirror_snapshot_with_timeout(
                &server_url,
                &source,
                &snapshot,
                &signer,
                TOTAL_TIMEOUT,
            )
            .await?;
            let request = completed_request(server).await?;

            assert!(request.head.starts_with("PUT /mirror HTTP/1.1\r\n"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.body)?,
                serde_json::json!({ "url": source })
            );
            assert_eq!(
                request_header(&request.head, "content-type"),
                Some("application/json")
            );
            assert_eq!(
                request_header(&request.head, "x-sha-256"),
                Some(snapshot.sha256.as_str())
            );
            assert_eq!(
                request_header(&request.head, "x-content-length"),
                Some(snapshot.size.to_string().as_str())
            );
            assert_eq!(
                request_header(&request.head, "x-content-type"),
                Some(snapshot.mime_type.as_str())
            );

            let authorization = request_header(&request.head, "authorization")
                .context("mirror omitted Authorization")?
                .strip_prefix("Nostr ")
                .context("mirror authorization used the wrong scheme")?;
            let event: Event = serde_json::from_slice(&STANDARD.decode(authorization)?)?;
            event.verify()?;
            assert_eq!(event.kind, Kind::BlossomAuth);
            assert_eq!(event.pubkey, signer_keys.public_key());
            assert_eq!(event_tag(&event, "t"), Some("upload"));
            assert_eq!(event_tag(&event, "x"), Some(snapshot.sha256.as_str()));
            assert!(event_tag(&event, "expiration").is_some());

            assert_eq!(mirrored.descriptor.sha256, snapshot.sha256);
            assert_eq!(mirrored.newly_stored, status == "201 Created");
        }
        Ok(())
    }

    #[tokio::test]
    async fn mirror_rejects_redirects_and_mismatched_descriptors() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let source = Url::parse(&format!("https://primary.example/{}.apk", snapshot.sha256))?;
        let signer = NgitSigner::Keys(Keys::generate());

        let (redirect_url, redirect_server) = spawn_one_shot_server(|base_url| TestResponse {
            status: "307 Temporary Redirect",
            headers: vec![("Location".to_owned(), format!("{base_url}/elsewhere"))],
            body: String::new(),
        })
        .await?;
        let error =
            mirror_snapshot_with_timeout(&redirect_url, &source, &snapshot, &signer, TOTAL_TIMEOUT)
                .await
                .unwrap_err();
        assert!(format!("{error:#}").contains("307 Temporary Redirect"));
        let request = completed_request(redirect_server).await?;
        assert!(request.head.starts_with("PUT /mirror HTTP/1.1\r\n"));

        let expected_mime = snapshot.mime_type.clone();
        let expected_size = snapshot.size;
        let (invalid_url, invalid_server) = spawn_one_shot_server(move |base_url| TestResponse {
            status: "201 Created",
            headers: Vec::new(),
            body: descriptor_json(base_url, &"0".repeat(64), expected_size, &expected_mime),
        })
        .await?;
        let error =
            mirror_snapshot_with_timeout(&invalid_url, &source, &snapshot, &signer, TOTAL_TIMEOUT)
                .await
                .unwrap_err();
        assert!(format!("{error:#}").contains("descriptor SHA-256"));
        completed_request(invalid_server).await?;
        Ok(())
    }

    #[tokio::test]
    async fn multi_server_upload_preserves_order_and_statuses() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let signer = NgitSigner::Keys(Keys::generate());
        let request_order = Arc::new(AtomicUsize::new(0));
        let mut servers = Vec::new();
        let mut tasks = Vec::new();

        for (index, status) in ["201 Created", "200 OK", "201 Created"]
            .into_iter()
            .enumerate()
        {
            let expected_hash = snapshot.sha256.clone();
            let expected_mime = snapshot.mime_type.clone();
            let expected_size = snapshot.size;
            let request_order = Arc::clone(&request_order);
            let (server_url, task) = spawn_observed_server(
                move |base_url| TestResponse {
                    status,
                    headers: Vec::new(),
                    body: descriptor_json(base_url, &expected_hash, expected_size, &expected_mime),
                },
                move || {
                    let observed = request_order.fetch_add(1, Ordering::SeqCst);
                    if observed != index {
                        bail!("server {index} was contacted at position {observed}");
                    }
                    Ok(())
                },
            )
            .await?;
            servers.push(Url::parse(&server_url)?);
            tasks.push(task);
        }

        let uploaded = upload_snapshot_to_servers(&servers, &snapshot, &signer).await?;
        let requests =
            futures::future::try_join_all(tasks.into_iter().map(completed_request)).await?;

        assert!(requests[0].head.starts_with("PUT /upload HTTP/1.1\r\n"));
        assert!(requests[1].head.starts_with("PUT /mirror HTTP/1.1\r\n"));
        assert!(requests[2].head.starts_with("PUT /mirror HTTP/1.1\r\n"));
        assert_eq!(
            uploaded
                .servers
                .iter()
                .map(|outcome| outcome.status)
                .collect::<Vec<_>>(),
            [
                BlossomServerStatus::Stored,
                BlossomServerStatus::AlreadyPresent,
                BlossomServerStatus::Stored,
            ]
        );
        assert_eq!(
            uploaded.primary.url,
            uploaded.servers[0].descriptor.as_ref().unwrap().url
        );
        let json = serde_json::to_value(&uploaded)?;
        assert_eq!(json["servers"][0]["operation"], "upload");
        assert_eq!(json["servers"][1]["operation"], "mirror");
        assert_eq!(json["servers"][0]["status"], "stored");
        assert_eq!(json["servers"][1]["status"], "already_present");
        assert_eq!(request_order.load(Ordering::SeqCst), 3);
        Ok(())
    }

    #[tokio::test]
    async fn failure_retains_plan_and_only_new_blobs_as_orphans() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let signer = NgitSigner::Keys(Keys::generate());

        let expected_hash = snapshot.sha256.clone();
        let expected_mime = snapshot.mime_type.clone();
        let expected_size = snapshot.size;
        let (primary_url, primary_task) = spawn_one_shot_server(move |base_url| TestResponse {
            status: "201 Created",
            headers: Vec::new(),
            body: descriptor_json(base_url, &expected_hash, expected_size, &expected_mime),
        })
        .await?;
        let expected_hash = snapshot.sha256.clone();
        let expected_mime = snapshot.mime_type.clone();
        let expected_size = snapshot.size;
        let (existing_url, existing_task) = spawn_one_shot_server(move |base_url| TestResponse {
            status: "200 OK",
            headers: Vec::new(),
            body: descriptor_json(base_url, &expected_hash, expected_size, &expected_mime),
        })
        .await?;
        let expected_mime = snapshot.mime_type.clone();
        let expected_size = snapshot.size;
        let (rejected_url, rejected_task) = spawn_one_shot_server(move |base_url| TestResponse {
            status: "201 Created",
            headers: Vec::new(),
            body: descriptor_json(base_url, &"0".repeat(64), expected_size, &expected_mime),
        })
        .await?;
        let untouched = Url::parse("http://127.0.0.1:9/")?;
        let servers = [
            Url::parse(&primary_url)?,
            Url::parse(&existing_url)?,
            Url::parse(&rejected_url)?,
            untouched,
        ];

        let error = upload_snapshot_to_servers(&servers, &snapshot, &signer)
            .await
            .unwrap_err();
        completed_request(primary_task).await?;
        completed_request(existing_task).await?;
        completed_request(rejected_task).await?;

        assert_eq!(
            error
                .servers
                .iter()
                .map(|outcome| outcome.status)
                .collect::<Vec<_>>(),
            [
                BlossomServerStatus::Stored,
                BlossomServerStatus::AlreadyPresent,
                BlossomServerStatus::Failed,
                BlossomServerStatus::NotAttempted,
            ]
        );
        assert_eq!(error.possible_orphan_blobs.len(), 2);
        assert_eq!(error.possible_orphan_blobs[0].server, servers[0]);
        assert!(error.possible_orphan_blobs[0].url.is_some());
        assert_eq!(error.possible_orphan_blobs[1].server, servers[2]);
        assert!(error.possible_orphan_blobs[1].url.is_none());
        assert!(
            error
                .possible_orphan_blobs
                .iter()
                .all(|orphan| orphan.server != servers[1])
        );
        let json = serde_json::to_value(&error)?;
        assert_eq!(json["servers"][2]["status"], "failed");
        assert_eq!(json["servers"][3]["status"], "not_attempted");
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_mirror_is_unknown_and_possible_orphan() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let signer = NgitSigner::Keys(Keys::generate());

        let expected_hash = snapshot.sha256.clone();
        let expected_mime = snapshot.mime_type.clone();
        let expected_size = snapshot.size;
        let (primary_url, primary_task) = spawn_one_shot_server(move |base_url| TestResponse {
            status: "200 OK",
            headers: Vec::new(),
            body: descriptor_json(base_url, &expected_hash, expected_size, &expected_mime),
        })
        .await?;
        let (ambiguous_url, ambiguous_task) = spawn_dropped_response_server().await?;
        let servers = [
            Url::parse(&primary_url)?,
            Url::parse(&ambiguous_url)?,
            Url::parse("http://127.0.0.1:9/")?,
        ];

        let error = upload_snapshot_to_servers(&servers, &snapshot, &signer)
            .await
            .unwrap_err();
        completed_request(primary_task).await?;
        let request = completed_request(ambiguous_task).await?;

        assert!(request.head.starts_with("PUT /mirror HTTP/1.1\r\n"));
        assert_eq!(
            error
                .servers
                .iter()
                .map(|outcome| outcome.status)
                .collect::<Vec<_>>(),
            [
                BlossomServerStatus::AlreadyPresent,
                BlossomServerStatus::Unknown,
                BlossomServerStatus::NotAttempted,
            ]
        );
        assert_eq!(
            error.possible_orphan_blobs,
            [PossibleOrphanBlob {
                server: servers[1].clone(),
                sha256: snapshot.sha256.clone(),
                url: None,
            }]
        );
        Ok(())
    }

    #[tokio::test]
    async fn upload_does_not_follow_authenticated_redirects() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_one_shot_server(|base_url| TestResponse {
            status: "307 Temporary Redirect",
            headers: vec![("Location".to_owned(), format!("{base_url}/elsewhere"))],
            body: String::new(),
        })
        .await?;

        let error = upload_snapshot_with_timeout(
            &server_url,
            &snapshot,
            &NgitSigner::Keys(Keys::generate()),
            TOTAL_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("307 Temporary Redirect"));
        let request = completed_request(server).await?;
        assert!(request.head.starts_with("PUT /upload HTTP/1.1\r\n"));
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_authentication_and_payment_responses_are_actionable() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let signer = NgitSigner::Keys(Keys::generate());

        for (status, body, expected) in [
            (
                "401 Unauthorized",
                "challenge required",
                "authentication challenge flows are not supported",
            ),
            (
                "402 Payment Required",
                "invoice required",
                "paid Blossom uploads are not supported",
            ),
        ] {
            let (server_url, server) = spawn_one_shot_server(move |_| TestResponse {
                status,
                headers: Vec::new(),
                body: body.to_owned(),
            })
            .await?;

            let error =
                upload_snapshot_with_timeout(&server_url, &snapshot, &signer, TOTAL_TIMEOUT)
                    .await
                    .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(expected));
            assert!(message.contains(body));
            completed_request(server).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn server_errors_leave_storage_unknown_and_report_a_possible_orphan() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let (server_url, server) = spawn_one_shot_server(move |_| TestResponse {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "descriptor persistence failed".to_owned(),
        })
        .await?;
        let server_url = Url::parse(&server_url)?;

        let error = upload_snapshot_to_servers(
            std::slice::from_ref(&server_url),
            &snapshot,
            &NgitSigner::Keys(Keys::generate()),
        )
        .await
        .unwrap_err();
        completed_request(server).await?;

        assert_eq!(error.servers[0].status, BlossomServerStatus::Unknown);
        assert_eq!(error.possible_orphan_blobs.len(), 1);
        assert_eq!(error.possible_orphan_blobs[0].server, server_url);
        assert_eq!(error.possible_orphan_blobs[0].sha256, snapshot.sha256);
        assert!(error.possible_orphan_blobs[0].url.is_none());
        assert!(error.message.contains("blob storage is uncertain"));
        Ok(())
    }

    #[tokio::test]
    async fn rejected_response_bodies_are_terminal_safe() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let body = "\x1b[31mred\u{202e}evil\u{0007}\nnext";
        let (server_url, server) = spawn_one_shot_server(move |_| TestResponse {
            status: "400 Bad Request",
            headers: Vec::new(),
            body: body.to_owned(),
        })
        .await?;

        let error = upload_snapshot_with_timeout(
            &server_url,
            &snapshot,
            &NgitSigner::Keys(Keys::generate()),
            TOTAL_TIMEOUT,
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("[31mredevil next"));
        assert!(!message.contains('\x1b'));
        assert!(!message.contains('\u{202e}'));
        assert!(!message.contains('\u{0007}'));
        completed_request(server).await?;
        Ok(())
    }

    #[tokio::test]
    async fn html_error_pages_are_not_echoed_to_the_terminal() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let body = "<!doctype html><style>very long hosted-service page</style>";
        let (server_url, server) = spawn_one_shot_server(move |_| TestResponse {
            status: "404 Not Found",
            headers: vec![(
                "Content-Type".to_owned(),
                "text/html; charset=utf-8".to_owned(),
            )],
            body: body.to_owned(),
        })
        .await?;

        let error = upload_snapshot_with_timeout(
            &server_url,
            &snapshot,
            &NgitSigner::Keys(Keys::generate()),
            TOTAL_TIMEOUT,
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("HTML error page instead of a Blossom response"));
        assert!(!message.contains("very long hosted-service page"));
        completed_request(server).await?;
        Ok(())
    }

    #[tokio::test]
    async fn total_timeout_covers_the_complete_descriptor_body() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let server_url = format!("http://{}", listener.local_addr()?);
        let (headers_sent, headers_received) = tokio::sync::oneshot::channel();
        let mut server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(SERVER_TIMEOUT, listener.accept())
                .await
                .context("timed out waiting for Blossom request")??;
            read_request(&mut stream).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{")
                .await?;
            let _ = headers_sent.send(());
            std::future::pending::<Result<()>>().await
        });
        let signer = NgitSigner::Keys(Keys::generate());
        let upload =
            upload_snapshot_with_timeout(&server_url, &snapshot, &signer, Duration::from_secs(1));
        tokio::pin!(upload);

        tokio::select! {
            result = &mut upload => {
                server.abort();
                bail!("upload completed before the partial descriptor was observed: {result:?}");
            }
            observed = tokio::time::timeout(SERVER_TIMEOUT, headers_received) => {
                observed.context("timed out waiting for partial descriptor")??;
            }
        }
        let error = upload.await.unwrap_err();
        assert!(format!("{error:#}").contains("total timeout"));
        server.abort();
        let _ = (&mut server).await;
        Ok(())
    }

    #[tokio::test]
    async fn upload_rejects_descriptors_that_do_not_commit_to_snapshot() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"release")?;
        let snapshot = snapshot_local_file(LocalFileRequest::new(file.path())).await?;
        let signer = NgitSigner::Keys(Keys::generate());

        let cases = [
            ("sha", "descriptor SHA-256"),
            ("size", "descriptor size"),
            ("mime", "descriptor MIME type"),
            ("url", "descriptor URL does not identify"),
            ("url_query", "URL must not contain a query"),
        ];
        for (field, expected_error) in cases {
            let expected_hash = snapshot.sha256.clone();
            let expected_mime = snapshot.mime_type.clone();
            let expected_size = snapshot.size;
            let field = field.to_owned();
            let response_field = field.clone();
            let (server_url, server) = spawn_one_shot_server(move |base_url| {
                let mut value: serde_json::Value = serde_json::from_str(&descriptor_json(
                    base_url,
                    &expected_hash,
                    expected_size,
                    &expected_mime,
                ))
                .expect("test descriptor is valid JSON");
                match response_field.as_str() {
                    "sha" => value["sha256"] = serde_json::json!("0".repeat(64)),
                    "size" => value["size"] = serde_json::json!(expected_size + 1),
                    "mime" => value["type"] = serde_json::json!("text/plain"),
                    "url" => {
                        value["url"] = serde_json::json!(format!("{base_url}/not-the-hash.apk"));
                    }
                    "url_query" => {
                        value["url"] = serde_json::json!(format!(
                            "{base_url}/{expected_hash}.apk?token=secret"
                        ));
                    }
                    _ => unreachable!(),
                }
                TestResponse {
                    status: "200 OK",
                    headers: Vec::new(),
                    body: value.to_string(),
                }
            })
            .await?;

            let error =
                upload_snapshot_with_timeout(&server_url, &snapshot, &signer, TOTAL_TIMEOUT)
                    .await
                    .unwrap_err();
            assert!(
                format!("{error:#}").contains(expected_error),
                "wrong error for {field}: {error:#}"
            );
            completed_request(server).await?;
        }
        Ok(())
    }
}
