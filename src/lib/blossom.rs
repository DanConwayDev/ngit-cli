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
};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin_hashes::{HashEngine as _, sha256};
use nostr::prelude::{Event, EventId, Filter, Kind, PublicKey, Timestamp, Url};
use tempfile::NamedTempFile;

use crate::{
    event_ordering::latest_event,
    release_download::{DEFAULT_MAX_ASSET_BYTES, DownloadWarning, infer_mime_type},
};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// A local file which should be staged for a Blossom upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFileRequest {
    pub source_path: PathBuf,
    /// Caller-supplied MIME type. The filename extension is used when absent.
    pub mime_type: Option<String>,
    /// Maximum number of bytes copied into the stable snapshot.
    pub max_bytes: u64,
}

impl LocalFileRequest {
    pub fn new(source_path: impl Into<PathBuf>) -> Self {
        Self {
            source_path: source_path.into(),
            mime_type: None,
            max_bytes: DEFAULT_MAX_ASSET_BYTES,
        }
    }
}

/// Immutable bytes and metadata prepared for one or more Blossom uploads.
#[derive(Debug)]
pub struct FileSnapshot {
    _file: NamedTempFile,
    pub filename: String,
    pub mime_type: String,
    /// Lowercase, 64-character SHA-256 of the snapshotted bytes.
    pub sha256: String,
    pub size: u64,
    pub warnings: Vec<DownloadWarning>,
}

impl FileSnapshot {
    #[cfg(test)]
    fn reopen(&self) -> Result<File> {
        self._file
            .reopen()
            .context("failed to reopen the stable asset snapshot")
    }
}

/// Copy a local regular file into a stable, bounded snapshot without retaining
/// its complete contents in memory.
pub async fn snapshot_local_file(request: LocalFileRequest) -> Result<FileSnapshot> {
    if request.max_bytes == 0 {
        bail!("asset byte limit must be greater than zero");
    }

    let filename = source_filename(&request.source_path)?;
    let mime = infer_mime_type(request.mime_type.as_deref(), None, &filename)?;

    tokio::task::spawn_blocking(move || snapshot_local_file_sync(request, filename, mime))
        .await
        .context("local asset snapshot task failed")?
}

fn snapshot_local_file_sync(
    request: LocalFileRequest,
    filename: String,
    mime: crate::release_download::MimeResolution,
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
        _file: snapshot,
        filename,
        mime_type: mime.mime_type,
        sha256: sha256::Hash::from_engine(engine).to_string(),
        size,
        warnings: mime.warnings,
    })
}

fn source_filename(path: &Path) -> Result<String> {
    let filename = path
        .file_name()
        .ok_or_else(|| anyhow!("local asset path has no filename"))?
        .to_str()
        .ok_or_else(|| anyhow!("local asset filename is not valid UTF-8"))?;
    if filename.is_empty() || filename.chars().any(char::is_control) {
        bail!("local asset filename is empty or unsafe");
    }
    Ok(filename.to_owned())
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
    use std::io::Read as _;

    use anyhow::Result;
    use nostr::prelude::{
        EventBuilder, Keys, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
    };
    use tempfile::tempdir;

    use super::*;

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
}
