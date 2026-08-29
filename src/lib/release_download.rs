//! Bounded, streaming acquisition of URL-backed software release assets.
//!
//! The downloader deliberately retains no asset body. It observes the exact
//! bytes returned by the server, incrementally computes their SHA-256 and
//! length, and returns only the metadata needed to construct a NIP-82 asset.

use std::{
    error::Error,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin_hashes::{HashEngine as _, sha256};
use reqwest::{
    Url,
    header::{ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_TYPE},
    redirect::Policy,
};
use serde::Serialize;

use crate::software_release::valid_mime_essence;

/// Default upper bound for a single downloaded asset (4 GiB).
pub const DEFAULT_MAX_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Network and resource limits applied to one asset download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadLimits {
    /// Maximum number of redirect responses which may be followed.
    pub max_redirects: usize,
    /// Maximum time spent establishing an individual connection.
    pub connect_timeout: Duration,
    /// Maximum time allowed between response-body chunks.
    pub idle_timeout: Duration,
    /// Maximum time for the request, redirects, and complete response body.
    pub total_timeout: Duration,
    /// Maximum number of response-body bytes to hash.
    pub max_bytes: u64,
    /// Permit an HTTPS URL to redirect to unencrypted HTTP.
    pub allow_https_downgrade: bool,
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            max_redirects: 5,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(30 * 60),
            max_bytes: DEFAULT_MAX_ASSET_BYTES,
            allow_https_downgrade: false,
        }
    }
}

/// A URL-backed asset to acquire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UrlAssetRequest {
    /// Stable URL which will be published in the asset event.
    pub source_url: String,
    /// Caller-supplied filename, taking precedence over HTTP and URL hints.
    pub filename: Option<String>,
    /// Caller-supplied MIME type, taking precedence over HTTP and extension
    /// hints.
    pub mime_type: Option<String>,
    /// Network and byte limits for this request.
    pub limits: DownloadLimits,
}

impl UrlAssetRequest {
    pub fn new(source_url: impl Into<String>) -> Self {
        Self {
            source_url: source_url.into(),
            filename: None,
            mime_type: None,
            limits: DownloadLimits::default(),
        }
    }
}

/// Stable warning categories for human output and the JSON API.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadWarningCode {
    Redirected,
    NonPublicHost,
    SuspiciousUrlQuery,
    FilenameSanitized,
    FilenameHintIgnored,
    FilenameFallback,
    MimeParametersIgnored,
    InvalidMimeHint,
    MimeConflict,
    GenericMime,
    ContentLengthMismatch,
}

/// A non-fatal acquisition or metadata diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DownloadWarning {
    pub code: DownloadWarningCode,
    pub message: String,
}

impl DownloadWarning {
    fn new(code: DownloadWarningCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Metadata observed while streaming a URL-backed asset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DownloadedAsset {
    /// Stable URL supplied by the caller. Redirects do not replace it.
    pub source_url: String,
    /// URL from which the response body was ultimately retrieved.
    pub final_url: String,
    pub filename: String,
    pub mime_type: String,
    /// Lowercase, 64-character SHA-256 of the downloaded bytes.
    pub sha256: String,
    /// Checked count of the downloaded bytes.
    #[serde(serialize_with = "serialize_u64_as_decimal")]
    pub size: u64,
    pub warnings: Vec<DownloadWarning>,
}

/// The result of MIME inference without performing a download.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MimeResolution {
    pub mime_type: String,
    pub warnings: Vec<DownloadWarning>,
}

/// Download an HTTP(S) asset without buffering its body in memory.
pub async fn download_url_asset(request: UrlAssetRequest) -> Result<DownloadedAsset> {
    validate_limits(request.limits)?;

    let source_url = Url::parse(&request.source_url).context("invalid asset source URL")?;
    validate_publication_url(&source_url)?;

    let total_timeout = request.limits.total_timeout;
    match tokio::time::timeout(total_timeout, download_url_asset_inner(request, source_url)).await {
        Ok(result) => result,
        Err(_) => bail!(
            "asset download exceeded its total timeout of {} seconds",
            total_timeout.as_secs()
        ),
    }
}

async fn download_url_asset_inner(
    request: UrlAssetRequest,
    source_url: Url,
) -> Result<DownloadedAsset> {
    let limits = request.limits;
    let redirect_policy = Policy::custom(move |attempt| {
        if attempt.previous().len() > limits.max_redirects {
            return attempt.error(RedirectPolicyError("too many redirects"));
        }

        if !is_http_scheme(attempt.url().scheme()) {
            return attempt.error(RedirectPolicyError(
                "redirect target must use HTTP or HTTPS",
            ));
        }
        if has_credentials(attempt.url()) {
            return attempt.error(RedirectPolicyError(
                "redirect target must not contain credentials",
            ));
        }
        if !limits.allow_https_downgrade
            && attempt
                .previous()
                .last()
                .is_some_and(|previous| previous.scheme() == "https")
            && attempt.url().scheme() == "http"
        {
            return attempt.error(RedirectPolicyError("refusing an HTTPS-to-HTTP redirect"));
        }

        attempt.follow()
    });

    let client = crate::tls::http_client_builder()
        .connect_timeout(limits.connect_timeout)
        .read_timeout(limits.idle_timeout)
        .redirect(redirect_policy)
        .build()
        .context("failed to create the asset HTTP client")?;

    // Request the identity representation so the bytes hashed here are not
    // silently changed by transparent content decoding.
    let mut response = client
        .get(source_url.clone())
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("failed to request asset URL")?
        .error_for_status()
        .context("asset server returned an unsuccessful status")?;

    let final_url = response.url().clone();
    validate_publication_url(&final_url)?;

    let advertised_size = response.content_length();
    if advertised_size.is_some_and(|size| size > limits.max_bytes) {
        bail!(
            "asset Content-Length exceeds the configured {} byte limit",
            limits.max_bytes
        );
    }

    let content_disposition = response
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut engine = sha256::Hash::engine();
    let mut size = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed while streaming the asset response")?
    {
        let chunk_size = u64::try_from(chunk.len())
            .map_err(|_| anyhow!("asset response chunk length does not fit in u64"))?;
        size = size
            .checked_add(chunk_size)
            .ok_or_else(|| anyhow!("asset byte length overflowed u64"))?;
        if size > limits.max_bytes {
            bail!(
                "asset exceeds the configured {} byte limit",
                limits.max_bytes
            );
        }
        engine.input(&chunk);
    }
    let sha256 = sha256::Hash::from_engine(engine).to_string();
    for url in [&source_url, &final_url] {
        if embedded_sha256(url).is_some_and(|expected| expected != sha256) {
            bail!("asset bytes do not match the SHA-256 embedded in their URL");
        }
    }

    let mut warnings = url_warnings(&source_url);
    if final_url != source_url {
        warnings.push(DownloadWarning::new(
            DownloadWarningCode::Redirected,
            "asset URL redirected; the source URL remains the published location",
        ));
        append_url_warnings(&mut warnings, &final_url);
    }
    if advertised_size.is_some_and(|advertised| advertised != size) {
        warnings.push(DownloadWarning::new(
            DownloadWarningCode::ContentLengthMismatch,
            "the response Content-Length did not match the streamed byte count",
        ));
    }

    let filename = resolve_filename(
        request.filename.as_deref(),
        content_disposition.as_deref(),
        &final_url,
        &sha256,
        &mut warnings,
    )?;
    let mime = infer_mime_type(
        request.mime_type.as_deref(),
        content_type.as_deref(),
        &filename,
    )?;
    warnings.extend(mime.warnings);

    Ok(DownloadedAsset {
        source_url: source_url.to_string(),
        final_url: final_url.to_string(),
        filename,
        mime_type: mime.mime_type,
        sha256,
        size,
        warnings,
    })
}

fn validate_limits(limits: DownloadLimits) -> Result<()> {
    if limits.max_bytes == 0 {
        bail!("asset byte limit must be greater than zero");
    }
    if limits.connect_timeout.is_zero()
        || limits.idle_timeout.is_zero()
        || limits.total_timeout.is_zero()
    {
        bail!("asset download timeouts must be greater than zero");
    }
    Ok(())
}

fn validate_publication_url(url: &Url) -> Result<()> {
    if !is_http_scheme(url.scheme()) {
        bail!("asset source URL must use HTTP or HTTPS");
    }
    if has_credentials(url) {
        bail!("asset source URL must not contain embedded credentials");
    }
    if url.host_str().is_none() {
        bail!("asset source URL must contain a host");
    }
    Ok(())
}

fn is_http_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https")
}

fn has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn embedded_sha256(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    let candidate = segment
        .split_once('.')
        .filter(|(_, extension)| !extension.is_empty())
        .map_or(segment, |(digest, _)| digest);
    (candidate.len() == 64 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| candidate.to_ascii_lowercase())
}

fn url_warnings(url: &Url) -> Vec<DownloadWarning> {
    let mut warnings = Vec::new();
    append_url_warnings(&mut warnings, url);
    warnings
}

fn append_url_warnings(warnings: &mut Vec<DownloadWarning>, url: &Url) {
    if is_non_public_host(url) && !has_warning(warnings, DownloadWarningCode::NonPublicHost) {
        warnings.push(DownloadWarning::new(
            DownloadWarningCode::NonPublicHost,
            "asset URL uses a local or non-public host and may be unreachable by consumers",
        ));
    }
    if has_suspicious_query(url) && !has_warning(warnings, DownloadWarningCode::SuspiciousUrlQuery)
    {
        warnings.push(DownloadWarning::new(
            DownloadWarningCode::SuspiciousUrlQuery,
            "asset URL query resembles a credential or expiring signature and will become public",
        ));
    }
}

fn has_warning(warnings: &[DownloadWarning], code: DownloadWarningCode) -> bool {
    warnings.iter().any(|warning| warning.code == code)
}

fn is_non_public_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }

    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => is_non_public_ipv4(address),
        Ok(IpAddr::V6(address)) => is_non_public_ipv6(address),
        Err(_) => false,
    }
}

fn is_non_public_ipv4(address: Ipv4Addr) -> bool {
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
}

fn is_non_public_ipv6(address: Ipv6Addr) -> bool {
    address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (address.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
        || (address.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
}

fn has_suspicious_query(url: &Url) -> bool {
    url.query_pairs().any(|(name, _)| {
        let name = name.to_ascii_lowercase();
        name.contains("token")
            || name.contains("secret")
            || name.contains("signature")
            || matches!(
                name.as_str(),
                "auth" | "authorization" | "api_key" | "apikey" | "key" | "sig"
            )
    })
}

fn resolve_filename(
    explicit: Option<&str>,
    content_disposition: Option<&str>,
    final_url: &Url,
    sha256: &str,
    warnings: &mut Vec<DownloadWarning>,
) -> Result<String> {
    if let Some(explicit) = explicit {
        let filename = sanitize_filename(explicit)
            .ok_or_else(|| anyhow!("explicit asset filename is empty or unsafe"))?;
        if filename != explicit.trim() {
            warnings.push(DownloadWarning::new(
                DownloadWarningCode::FilenameSanitized,
                "the explicit asset filename was sanitized",
            ));
        }
        return Ok(filename);
    }

    if let Some(header) = content_disposition {
        if let Some(raw_filename) = filename_from_content_disposition(header) {
            if let Some(filename) = sanitize_filename(&raw_filename) {
                if filename != raw_filename.trim() {
                    warnings.push(DownloadWarning::new(
                        DownloadWarningCode::FilenameSanitized,
                        "the Content-Disposition filename was sanitized",
                    ));
                }
                return Ok(filename);
            }
            warnings.push(DownloadWarning::new(
                DownloadWarningCode::FilenameHintIgnored,
                "the Content-Disposition filename was empty or unsafe and was ignored",
            ));
        }
    }

    if let Some(raw_filename) = filename_from_url(final_url) {
        if let Some(filename) = sanitize_filename(&raw_filename) {
            if filename != raw_filename.trim() {
                warnings.push(DownloadWarning::new(
                    DownloadWarningCode::FilenameSanitized,
                    "the asset URL filename was sanitized",
                ));
            }
            return Ok(filename);
        }
    }

    warnings.push(DownloadWarning::new(
        DownloadWarningCode::FilenameFallback,
        "no usable filename was available; a hash-derived filename was used",
    ));
    Ok(format!("asset-{}", &sha256[..12]))
}

/// Extract a filename hint from a Content-Disposition header.
///
/// RFC 5987's UTF-8 `filename*` form takes precedence over `filename`.
pub fn filename_from_content_disposition(header: &str) -> Option<String> {
    let parameters = split_header_parameters(header);
    let mut regular = None;
    for parameter in parameters.into_iter().skip(1) {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = unquote_header_value(value.trim());
        if name.eq_ignore_ascii_case("filename*") {
            if let Some(filename) = decode_extended_filename(&value) {
                return Some(filename);
            }
        } else if name.eq_ignore_ascii_case("filename") && regular.is_none() {
            regular = Some(value);
        }
    }
    regular.filter(|filename| !filename.is_empty())
}

fn split_header_parameters(header: &str) -> Vec<String> {
    let mut parameters = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in header.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parameters.push(header[start..index].trim().to_owned());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parameters.push(header[start..].trim().to_owned());
    parameters
}

fn unquote_header_value(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };

    let mut unquoted = String::with_capacity(inner.len());
    let mut escaped = false;
    for character in inner.chars() {
        if escaped {
            unquoted.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            unquoted.push(character);
        }
    }
    if escaped {
        unquoted.push('\\');
    }
    unquoted
}

fn decode_extended_filename(value: &str) -> Option<String> {
    let (charset, remainder) = value.split_once('\'')?;
    let (_, encoded) = remainder.split_once('\'')?;
    if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("us-ascii") {
        return None;
    }
    urlencoding::decode(encoded)
        .ok()
        .map(|value| value.into_owned())
}

fn filename_from_url(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    urlencoding::decode(segment)
        .ok()
        .map(|value| value.into_owned())
}

/// Reduce an untrusted filename to a bounded, display-safe basename.
pub fn sanitize_filename(filename: &str) -> Option<String> {
    let basename = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if basename.is_empty() || matches!(basename, "." | "..") {
        return None;
    }

    let mut sanitized = String::with_capacity(basename.len().min(255));
    for character in basename.chars() {
        let unsafe_character = character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
            || matches!(
                character,
                '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            );
        sanitized.push(if unsafe_character { '_' } else { character });
    }

    let trimmed_len = sanitized.trim_end_matches([' ', '.']).len();
    sanitized.truncate(trimmed_len);
    truncate_utf8(&mut sanitized, 255);
    if sanitized.is_empty() || matches!(sanitized.as_str(), "." | "..") {
        return None;
    }

    let stem = sanitized
        .split_once('.')
        .map_or(sanitized.as_str(), |(stem, _)| stem);
    if is_windows_reserved_name(stem) {
        sanitized.insert(0, '_');
        truncate_utf8(&mut sanitized, 255);
    }
    Some(sanitized)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

fn is_windows_reserved_name(stem: &str) -> bool {
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

/// Resolve a MIME type from explicit input, a Content-Type header, and a
/// filename extension, in that order.
pub fn infer_mime_type(
    explicit: Option<&str>,
    content_type: Option<&str>,
    filename: &str,
) -> Result<MimeResolution> {
    let mut warnings = Vec::new();
    let extension_mime = mime_type_from_filename(filename);
    let header_mime = content_type.and_then(|value| {
        let (normalized, had_parameters) = normalize_mime(value)?;
        if had_parameters {
            warnings.push(DownloadWarning::new(
                DownloadWarningCode::MimeParametersIgnored,
                "Content-Type parameters were omitted from the asset MIME type",
            ));
        }
        if valid_mime_essence(&normalized) {
            Some(normalized)
        } else {
            warnings.push(DownloadWarning::new(
                DownloadWarningCode::InvalidMimeHint,
                "the response Content-Type was invalid and was ignored",
            ));
            None
        }
    });

    let selected = if let Some(explicit) = explicit {
        let Some((normalized, had_parameters)) = normalize_mime(explicit) else {
            bail!("explicit asset MIME type is empty");
        };
        if !valid_mime_essence(&normalized) {
            bail!("explicit asset MIME type is invalid");
        }
        if had_parameters {
            warnings.push(DownloadWarning::new(
                DownloadWarningCode::MimeParametersIgnored,
                "parameters were omitted from the explicit asset MIME type",
            ));
        }
        for hint in [header_mime.as_deref(), extension_mime] {
            if hint.is_some_and(|hint| hint != normalized && !is_generic_mime(hint)) {
                push_mime_conflict(&mut warnings);
                break;
            }
        }
        normalized
    } else if let Some(header) = header_mime {
        if is_generic_mime(&header) {
            extension_mime.unwrap_or(header.as_str()).to_owned()
        } else {
            if extension_mime.is_some_and(|extension| extension != header) {
                push_mime_conflict(&mut warnings);
            }
            header
        }
    } else if let Some(extension) = extension_mime {
        extension.to_owned()
    } else {
        "application/octet-stream".to_owned()
    };

    if is_generic_mime(&selected) {
        warnings.push(DownloadWarning::new(
            DownloadWarningCode::GenericMime,
            "the asset MIME type could not be inferred more precisely than application/octet-stream",
        ));
    }

    Ok(MimeResolution {
        mime_type: selected,
        warnings,
    })
}

fn push_mime_conflict(warnings: &mut Vec<DownloadWarning>) {
    if !has_warning(warnings, DownloadWarningCode::MimeConflict) {
        warnings.push(DownloadWarning::new(
            DownloadWarningCode::MimeConflict,
            "asset MIME hints disagree; the higher-precedence value was used",
        ));
    }
}

fn normalize_mime(value: &str) -> Option<(String, bool)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (essence, parameters) = value
        .split_once(';')
        .map_or((value, false), |(essence, _)| (essence, true));
    Some((essence.trim().to_ascii_lowercase(), parameters))
}

fn is_generic_mime(value: &str) -> bool {
    value == "application/octet-stream"
}

fn serialize_u64_as_decimal<S>(value: &u64, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Return a known MIME type for a filename extension.
pub fn mime_type_from_filename(filename: &str) -> Option<&'static str> {
    let filename = filename.to_ascii_lowercase();
    let extension = filename.rsplit_once('.')?.1;
    match extension {
        "apk" => Some("application/vnd.android.package-archive"),
        "appimage" => Some("application/vnd.appimage"),
        "bz2" => Some("application/x-bzip2"),
        "deb" => Some("application/vnd.debian.binary-package"),
        "dmg" => Some("application/x-apple-diskimage"),
        "exe" => Some("application/vnd.microsoft.portable-executable"),
        "flatpak" => Some("application/vnd.flatpak"),
        "gz" | "tgz" => Some("application/gzip"),
        "jar" => Some("application/java-archive"),
        "json" => Some("application/json"),
        "msi" => Some("application/x-msi"),
        "pkg" => Some("application/vnd.apple.installer+xml"),
        "rpm" => Some("application/x-rpm"),
        "snap" => Some("application/vnd.snap"),
        "tar" => Some("application/x-tar"),
        "txt" => Some("text/plain"),
        "wasm" => Some("application/wasm"),
        "xz" => Some("application/x-xz"),
        "zip" => Some("application/zip"),
        "zst" => Some("application/zstd"),
        _ => None,
    }
}

#[derive(Debug)]
struct RedirectPolicyError(&'static str);

impl fmt::Display for RedirectPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for RedirectPolicyError {}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
        time::timeout,
    };

    use super::*;

    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(2);

    async fn spawn_one_shot_http_server(response: Vec<u8>) -> (SocketAddr, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test HTTP server");
        let address = listener.local_addr().expect("test HTTP server address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = timeout(TEST_IO_TIMEOUT, listener.accept())
                .await
                .expect("timed out waiting for asset request")
                .expect("accept asset request");
            let request = timeout(TEST_IO_TIMEOUT, async {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let count = stream.read(&mut buffer).await.expect("read asset request");
                    assert!(count > 0, "client closed before sending complete headers");
                    request.extend_from_slice(&buffer[..count]);
                    assert!(
                        request.len() <= 32 * 1024,
                        "asset request headers too large"
                    );
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                request
            })
            .await
            .expect("timed out reading asset request");
            timeout(TEST_IO_TIMEOUT, stream.write_all(&response))
                .await
                .expect("timed out writing asset response")
                .expect("write asset response");
            String::from_utf8(request).expect("HTTP request is UTF-8")
        });
        (address, task)
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (name, value) in headers {
            response.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        response.extend_from_slice(b"Connection: close\r\n\r\n");
        response.extend_from_slice(body);
        response
    }

    async fn completed_request(task: JoinHandle<String>) -> String {
        timeout(TEST_IO_TIMEOUT, task)
            .await
            .expect("timed out waiting for test HTTP server")
            .expect("test HTTP server task")
    }

    #[test]
    fn sanitizes_untrusted_filenames_to_a_bounded_basename() {
        assert_eq!(
            sanitize_filename("../../folder\\ngit\u{202e}.apk"),
            Some("ngit_.apk".to_owned())
        );
        assert_eq!(sanitize_filename("CON.exe"), Some("_CON.exe".to_owned()));
        assert_eq!(sanitize_filename(".."), None);

        let long = format!("{}界.zip", "a".repeat(260));
        let sanitized = sanitize_filename(&long).expect("filename");
        assert!(sanitized.len() <= 255);
        assert!(sanitized.is_char_boundary(sanitized.len()));
    }

    #[test]
    fn finds_bare_and_extension_suffixed_url_hashes() {
        let digest = "ABCDEF0000000000000000000000000000000000000000000000000000000000";
        let lowercase_digest = digest.to_ascii_lowercase();
        for path in [digest.to_owned(), format!("{digest}.tar.gz")] {
            let url = Url::parse(&format!("https://blossom.example/{path}"))
                .expect("content-addressed URL");
            assert_eq!(
                embedded_sha256(&url).as_deref(),
                Some(lowercase_digest.as_str())
            );
        }
        for path in [format!("{digest}."), format!("x{digest}.zip")] {
            let url = Url::parse(&format!("https://blossom.example/{path}"))
                .expect("non-content-addressed URL");
            assert_eq!(embedded_sha256(&url), None);
        }
    }

    #[test]
    fn parses_regular_and_extended_content_disposition_filenames() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"release; one.zip\""),
            Some("release; one.zip".to_owned())
        );
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename=fallback.zip; filename*=UTF-8''ngit-%E2%9C%93.zip"
            ),
            Some("ngit-✓.zip".to_owned())
        );
    }

    #[test]
    fn explicit_mime_wins_and_reports_conflicting_hints() {
        let resolved = infer_mime_type(
            Some("application/zip; charset=binary"),
            Some("application/gzip"),
            "release.tar.gz",
        )
        .expect("MIME resolution");

        assert_eq!(resolved.mime_type, "application/zip");
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.code == DownloadWarningCode::MimeParametersIgnored)
        );
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.code == DownloadWarningCode::MimeConflict)
        );
    }

    #[test]
    fn extension_improves_a_generic_or_missing_content_type() {
        let generic = infer_mime_type(None, Some("application/octet-stream"), "ngit-x86_64.tar.xz")
            .expect("MIME resolution");
        assert_eq!(generic.mime_type, "application/x-xz");
        assert!(!has_warning(
            &generic.warnings,
            DownloadWarningCode::GenericMime
        ));

        let unknown = infer_mime_type(None, None, "ngit.bin").expect("MIME resolution");
        assert_eq!(unknown.mime_type, "application/octet-stream");
        assert!(has_warning(
            &unknown.warnings,
            DownloadWarningCode::GenericMime
        ));
    }

    #[test]
    fn rejects_invalid_explicit_mime() {
        assert!(infer_mime_type(Some("not a mime"), None, "asset.bin").is_err());
        assert!(infer_mime_type(Some("a/b/c"), None, "asset.bin").is_err());
        assert!(infer_mime_type(Some("*/*"), None, "asset.bin").is_err());
    }

    #[tokio::test]
    async fn streams_body_and_reports_exact_observed_metadata() {
        let body = b"ngit release bytes\n";
        let content_length = body.len().to_string();
        let response = http_response(
            "200 OK",
            &[
                ("Content-Length", &content_length),
                (
                    "Content-Disposition",
                    "attachment; filename*=UTF-8''ngit-%E2%9C%93.apk",
                ),
                (
                    "Content-Type",
                    "application/vnd.android.package-archive; charset=binary",
                ),
            ],
            body,
        );
        let (address, server) = spawn_one_shot_http_server(response).await;
        let source_url = format!("http://{address}/ignored-name.bin");

        let downloaded = download_url_asset(UrlAssetRequest::new(&source_url))
            .await
            .expect("download asset");

        assert_eq!(downloaded.source_url, source_url);
        assert_eq!(downloaded.final_url, source_url);
        assert_eq!(downloaded.filename, "ngit-✓.apk");
        assert_eq!(
            downloaded.mime_type,
            "application/vnd.android.package-archive"
        );
        assert_eq!(
            downloaded.sha256,
            "7e073656b5f97f1c4077eb8ed67a9337068f4530c34fc95015749c05531b686a"
        );
        assert_eq!(downloaded.size, 19);
        assert!(has_warning(
            &downloaded.warnings,
            DownloadWarningCode::MimeParametersIgnored
        ));
        assert!(has_warning(
            &downloaded.warnings,
            DownloadWarningCode::NonPublicHost
        ));

        let request = completed_request(server).await;
        assert!(request.starts_with("GET /ignored-name.bin HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("accept-encoding: identity\r\n")
        );
    }

    #[tokio::test]
    async fn follows_redirect_but_preserves_the_published_source_url() {
        let body = b"redirected asset";
        let content_length = body.len().to_string();
        let final_response = http_response(
            "200 OK",
            &[
                ("Content-Length", &content_length),
                ("Content-Type", "application/zip"),
            ],
            body,
        );
        let (final_address, final_server) = spawn_one_shot_http_server(final_response).await;
        let location = format!("http://{final_address}/release.zip");
        let redirect_response = http_response("302 Found", &[("Location", &location)], b"");
        let (source_address, redirect_server) = spawn_one_shot_http_server(redirect_response).await;
        let source_url = format!("http://{source_address}/latest");

        let downloaded = download_url_asset(UrlAssetRequest::new(&source_url))
            .await
            .expect("download redirected asset");

        assert_eq!(downloaded.source_url, source_url);
        assert_eq!(downloaded.final_url, location);
        assert_eq!(downloaded.filename, "release.zip");
        assert_eq!(downloaded.mime_type, "application/zip");
        assert!(has_warning(
            &downloaded.warnings,
            DownloadWarningCode::Redirected
        ));
        completed_request(redirect_server).await;
        completed_request(final_server).await;
    }

    #[tokio::test]
    async fn rejects_an_advertised_body_larger_than_the_limit() {
        let response = http_response(
            "200 OK",
            &[("Content-Length", "6"), ("Content-Type", "text/plain")],
            b"",
        );
        let (address, server) = spawn_one_shot_http_server(response).await;
        let mut request = UrlAssetRequest::new(format!("http://{address}/asset.txt"));
        request.limits.max_bytes = 5;

        let error = download_url_asset(request)
            .await
            .expect_err("advertised size must be rejected");

        assert!(
            error
                .to_string()
                .contains("Content-Length exceeds the configured 5 byte limit"),
            "unexpected error: {error:#}"
        );
        completed_request(server).await;
    }

    #[tokio::test]
    async fn rejects_a_streamed_body_larger_than_the_limit() {
        let response = http_response(
            "200 OK",
            &[("Content-Type", "application/octet-stream")],
            b"abcdef",
        );
        let (address, server) = spawn_one_shot_http_server(response).await;
        let mut request = UrlAssetRequest::new(format!("http://{address}/asset.bin"));
        request.limits.max_bytes = 5;

        let error = download_url_asset(request)
            .await
            .expect_err("streamed size must be rejected");

        assert!(
            error
                .to_string()
                .contains("asset exceeds the configured 5 byte limit"),
            "unexpected error: {error:#}"
        );
        completed_request(server).await;
    }

    #[tokio::test]
    async fn rejects_bytes_that_do_not_match_an_extension_suffixed_url_sha256() {
        let body = b"not the advertised digest";
        let content_length = body.len().to_string();
        let response = http_response("200 OK", &[("Content-Length", &content_length)], body);
        let (address, server) = spawn_one_shot_http_server(response).await;
        let embedded_digest = "0000000000000000000000000000000000000000000000000000000000000000";
        let source_url = format!("http://{address}/{embedded_digest}.zip");

        let error = download_url_asset(UrlAssetRequest::new(source_url))
            .await
            .expect_err("digest mismatch must be rejected");

        assert!(
            error
                .to_string()
                .contains("do not match the SHA-256 embedded in their URL"),
            "unexpected error: {error:#}"
        );
        completed_request(server).await;
    }
}
