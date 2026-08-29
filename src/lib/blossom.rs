//! Blossom transport support for software release assets.
//!
//! Local files are copied into a private temporary file before an upload is
//! attempted. The snapshot makes the hash, size, and bytes sent to a Blossom
//! server one immutable unit even if the source path changes later.

use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bitcoin_hashes::{HashEngine as _, sha256};
use nostr::prelude::{Event, EventBuilder, EventId, Filter, Kind, PublicKey, Tag, Timestamp};
use reqwest::{
    StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue},
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
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

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

/// A blob which a failed workflow may have stored without publishing a release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PossibleOrphanBlob {
    pub server: Url,
    pub sha256: String,
    pub url: Option<Url>,
}

/// Successful result of uploading once and mirroring to every remaining server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MultiServerUpload {
    /// The first server's descriptor; this URL is suitable for a NIP-82 `url`
    /// tag.
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
}

impl BlobRequestError {
    fn definite(error: anyhow::Error, possible_orphan: bool) -> Self {
        Self {
            kind: RequestFailureKind::Definite,
            message: format!("{error:#}"),
            possible_orphan,
        }
    }

    fn unknown(error: anyhow::Error, possible_orphan: bool) -> Self {
        Self {
            kind: RequestFailureKind::Unknown,
            message: format!("{error:#}"),
            possible_orphan,
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
    let upload_url = blossom_endpoint_url(server_url, "upload")
        .map_err(|error| BlobRequestError::definite(error, false))?;
    let event = tokio::time::timeout_at(deadline, upload_authorization(snapshot, signer))
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
    let file = snapshot
        .reopen()
        .map_err(|error| BlobRequestError::definite(error, false))?;
    let body = reqwest::Body::wrap_stream(ReaderStream::new(tokio::fs::File::from_std(file)));

    let client = blossom_http_client().map_err(|error| BlobRequestError::definite(error, false))?;
    let request = client
        .put(upload_url)
        .header(CONTENT_LENGTH, snapshot.size)
        .header(CONTENT_TYPE, &snapshot.mime_type)
        .header("X-SHA-256", &snapshot.sha256)
        .header(AUTHORIZATION, authorization)
        .body(body);

    let response = tokio::time::timeout_at(deadline, request.send())
        .await
        .map_err(|_| {
            BlobRequestError::unknown(anyhow!("Blossom upload exceeded its total timeout"), true)
        })?
        .map_err(|error| classify_send_error(error, "upload"))?;

    read_store_response(response, snapshot, deadline, "upload").await
}

/// Upload to the first server, then mirror sequentially to every other server.
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
    let event = tokio::time::timeout_at(deadline, upload_authorization(snapshot, signer))
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
        .map_err(|error| classify_send_error(error, "mirror"))?;

    read_store_response(response, snapshot, deadline, "mirror").await
}

fn classify_send_error(error: reqwest::Error, operation: &str) -> BlobRequestError {
    let definitely_not_stored = error.is_builder() || error.is_connect();
    let error = anyhow!(error).context(format!("failed to send the Blossom {operation} request"));
    if definitely_not_stored {
        BlobRequestError::definite(error, false)
    } else {
        BlobRequestError::unknown(error, true)
    }
}

fn blossom_http_client() -> Result<reqwest::Client> {
    crate::tls::http_client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .context("failed to create the Blossom HTTP client")
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
                "authentication challenge flows are not supported for release uploads"
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
            BlobRequestError::unknown(error, true)
        } else {
            BlobRequestError::definite(error, false)
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

async fn upload_authorization(snapshot: &FileSnapshot, signer: &NgitSigner) -> Result<Event> {
    let expires = Timestamp::now() + AUTHORIZATION_LIFETIME;
    let builder = EventBuilder::new(Kind::BlossomAuth, "Authorize Blossom upload").tags([
        Tag::parse(["t", "upload"]).expect("static Blossom action tag is valid"),
        Tag::parse(["x", &snapshot.sha256]).expect("computed SHA-256 tag is valid"),
        Tag::expiration(expires),
    ]);
    signer
        .sign_event_builder_with_description(builder, "Blossom upload authorization")
        .await
        .context("failed to sign the Blossom upload authorization")
}

fn authorization_header(event: &Event) -> Result<HeaderValue> {
    let event = serde_json::to_vec(event).context("failed to encode Blossom authorization")?;
    // Padded standard Base64 is accepted by Base64url-capable reference
    // servers and by deployed servers which have not yet adopted BUD-11's
    // newer Base64url wording (notably blossom.primal.net).
    HeaderValue::from_str(&format!("Nostr {}", STANDARD.encode(event)))
        .context("failed to construct the Blossom Authorization header")
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
    if descriptor.mime_type != snapshot.mime_type {
        bail!("Blossom descriptor MIME type does not match the upload");
    }
    validate_blob_url(&descriptor.url, &snapshot.sha256)
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
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Result, anyhow, bail};
    use base64::engine::general_purpose::STANDARD;
    use nostr::prelude::{
        Event, EventBuilder, Keys, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
    };
    use tempfile::tempdir;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
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

    struct TestResponse {
        status: &'static str,
        headers: Vec<(String, String)>,
        body: String,
    }

    async fn spawn_one_shot_server(
        response: impl FnOnce(&str) -> TestResponse,
    ) -> Result<(String, JoinHandle<Result<CapturedRequest>>)> {
        spawn_observed_server(response, || Ok(())).await
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
    fn authorization_header_uses_padded_standard_base64() -> Result<()> {
        let keys = Keys::generate();
        let event = server_list_event(&keys, 1, "compatibility", []);
        let event_json = serde_json::to_vec(&event)?;
        let expected = STANDARD.encode(&event_json);
        let header = authorization_header(&event)?;
        let encoded = header
            .to_str()?
            .strip_prefix("Nostr ")
            .context("authorization header omitted the Nostr scheme")?;

        assert_eq!(encoded, expected);
        assert_eq!(STANDARD.decode(encoded)?, event_json);
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
