//! Blossom transport support for software release assets.
//!
//! Local files are copied into a private temporary file before an upload is
//! attempted. The snapshot makes the hash, size, and bytes sent to a Blossom
//! server one immutable unit even if the source path changes later.

use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin_hashes::{HashEngine as _, sha256};
use tempfile::NamedTempFile;

use crate::release_download::{DEFAULT_MAX_ASSET_BYTES, DownloadWarning, infer_mime_type};

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

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use anyhow::Result;
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
}
