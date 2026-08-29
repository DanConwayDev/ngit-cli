//! NIP-5A static-site manifests and immutable directory snapshots.

use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin_hashes::{HashEngine as _, sha256};
use nostr::prelude::{EventBuilder, Kind, Tag, Url};

use crate::blossom::{FileSnapshot, LocalFileRequest, snapshot_local_file};

pub const NSITE_ROOT_KIND: Kind = Kind::Custom(15_128);
pub const NSITE_NAMED_KIND: Kind = Kind::Custom(35_128);
pub const MAX_NAMED_SITE_IDENTIFIER_BYTES: usize = 13;

/// One stable local file and the absolute path published for it.
#[derive(Debug)]
pub struct NsiteFileSnapshot {
    pub path: String,
    pub snapshot: FileSnapshot,
}

/// Typed input for a root or named NIP-5A manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NsiteManifestInput {
    /// `None` publishes the root kind-15128 site; `Some` publishes kind 35128.
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub source: Option<String>,
    pub servers: Vec<Url>,
}

/// Snapshot every regular file below a directory in deterministic path order.
///
/// Symlinks are rejected instead of followed: a deployment directory is an
/// explicit byte boundary, and silently walking outside it would make the
/// manifest depend on ambient filesystem state.
pub async fn snapshot_nsite_directory(root: &Path) -> Result<Vec<NsiteFileSnapshot>> {
    let files = collect_site_files(root)?;
    let mut snapshots = Vec::with_capacity(files.len());
    for (source_path, public_path) in files {
        let snapshot = snapshot_local_file(LocalFileRequest::new(&source_path))
            .await
            .with_context(|| format!("failed to snapshot nsite path {public_path}"))?;
        snapshots.push(NsiteFileSnapshot {
            path: public_path,
            snapshot,
        });
    }
    Ok(snapshots)
}

/// Return one representative snapshot for each content hash.
pub fn unique_blob_snapshots(files: &[NsiteFileSnapshot]) -> Vec<&FileSnapshot> {
    let mut seen = HashSet::new();
    files
        .iter()
        .filter_map(|file| {
            seen.insert(file.snapshot.sha256.clone())
                .then_some(&file.snapshot)
        })
        .collect()
}

/// Compute the order-independent aggregate hash required by NIP-5A.
pub fn aggregate_hash(files: &[NsiteFileSnapshot]) -> String {
    let mut lines = files
        .iter()
        .map(|file| format!("{} {}\n", file.snapshot.sha256, file.path))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    let mut engine = sha256::Hash::engine();
    for line in lines {
        engine.input(line.as_bytes());
    }
    sha256::Hash::from_engine(engine).to_string()
}

/// Build a deterministic root or named NIP-5A manifest event.
pub fn manifest_event_builder(
    files: &[NsiteFileSnapshot],
    input: &NsiteManifestInput,
) -> Result<(EventBuilder, String)> {
    if files.is_empty() {
        bail!("an nsite manifest requires at least one file");
    }
    validate_manifest_input(input)?;

    let mut tags = Vec::with_capacity(files.len() + input.servers.len() + 6);
    let kind = if let Some(identifier) = &input.identifier {
        tags.push(Tag::parse(["d", identifier]).context("invalid nsite identifier tag")?);
        NSITE_NAMED_KIND
    } else {
        NSITE_ROOT_KIND
    };

    for file in files {
        tags.push(
            Tag::parse(["path", &file.path, &file.snapshot.sha256])
                .context("invalid nsite path tag")?,
        );
    }
    let aggregate = aggregate_hash(files);
    tags.push(
        Tag::parse(["x", aggregate.as_str(), "aggregate"])
            .context("invalid nsite aggregate tag")?,
    );
    for server in &input.servers {
        tags.push(Tag::parse(["server", server.as_str()]).context("invalid nsite server tag")?);
    }
    append_optional_tag(&mut tags, "title", input.title.as_deref())?;
    append_optional_tag(&mut tags, "description", input.description.as_deref())?;
    append_optional_tag(&mut tags, "source", input.source.as_deref())?;

    Ok((EventBuilder::new(kind, "").tags(tags), aggregate))
}

pub fn validate_named_site_identifier(identifier: &str) -> Result<()> {
    if identifier.is_empty() || identifier.len() > MAX_NAMED_SITE_IDENTIFIER_BYTES {
        bail!(
            "named nsite identifier must contain 1 to {} ASCII bytes",
            MAX_NAMED_SITE_IDENTIFIER_BYTES
        );
    }
    if identifier.ends_with('-')
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("named nsite identifier must match ^[a-z0-9-]{{1,13}}$ and must not end with '-'");
    }
    Ok(())
}

fn collect_site_files(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("failed to inspect nsite directory {}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("nsite publish path must be a directory, not a symlink");
    }

    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .with_context(|| format!("failed to read nsite directory {}", directory.display()))?;
        for entry in entries {
            let entry = entry.context("failed to read an nsite directory entry")?;
            let path = entry.path();
            let file_type = entry.file_type().with_context(|| {
                format!("failed to inspect nsite directory entry {}", path.display())
            })?;
            if file_type.is_symlink() {
                bail!(
                    "nsite directory must not contain symlinks: {}",
                    path.display()
                );
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .context("nsite file escaped the deployment directory")?;
                files.push((path.clone(), public_site_path(relative)?));
            } else {
                bail!(
                    "nsite directory contains a non-regular entry: {}",
                    path.display()
                );
            }
        }
    }
    files.sort_unstable_by(|left, right| left.1.cmp(&right.1));
    if files.is_empty() {
        bail!("nsite publish directory contains no regular files");
    }
    Ok(files)
}

fn public_site_path(relative: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| anyhow!("nsite paths must be valid UTF-8"))?;
                if value.is_empty() || value.contains(['/', '\\', '\0']) {
                    bail!("nsite path contains an unsafe component");
                }
                parts.push(value);
            }
            _ => bail!("nsite path contains a non-normal component"),
        }
    }
    if parts.is_empty() {
        bail!("nsite file path must not be empty");
    }
    Ok(format!("/{}", parts.join("/")))
}

fn validate_manifest_input(input: &NsiteManifestInput) -> Result<()> {
    if let Some(identifier) = &input.identifier {
        validate_named_site_identifier(identifier)?;
    }
    validate_optional_metadata("title", input.title.as_deref())?;
    validate_optional_metadata("description", input.description.as_deref())?;
    if let Some(source) = input.source.as_deref() {
        validate_optional_metadata("source", Some(source))?;
        let source_url = Url::parse(source).context("nsite source must be an absolute URL")?;
        if !matches!(source_url.scheme(), "https" | "nostr") {
            bail!("nsite source must use the https or nostr scheme");
        }
    }
    Ok(())
}

fn validate_optional_metadata(name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            bail!("nsite {name} must be non-empty, trimmed, and contain no control characters");
        }
    }
    Ok(())
}

fn append_optional_tag(tags: &mut Vec<Tag>, name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        tags.push(Tag::parse([name, value]).with_context(|| format!("invalid nsite {name} tag"))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use nostr::prelude::event::FinalizeUnsignedEvent as _;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn snapshots_nested_files_in_public_path_order() -> Result<()> {
        let directory = tempdir()?;
        fs::create_dir(directory.path().join("assets"))?;
        fs::write(directory.path().join("index.html"), b"home")?;
        fs::write(directory.path().join("assets/app.js"), b"app")?;

        let files = snapshot_nsite_directory(directory.path()).await?;

        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["/assets/app.js", "/index.html"]
        );
        assert_eq!(
            files[0].snapshot.sha256,
            sha256::Hash::hash(b"app").to_string()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_inside_the_site_boundary() -> Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempdir()?;
        fs::write(directory.path().join("index.html"), b"home")?;
        symlink("index.html", directory.path().join("alias.html"))?;

        let error = snapshot_nsite_directory(directory.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must not contain symlinks"));
        Ok(())
    }

    #[tokio::test]
    async fn aggregate_hash_is_independent_of_file_order() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("a.txt"), b"a")?;
        fs::write(directory.path().join("b.txt"), b"b")?;
        let mut files = snapshot_nsite_directory(directory.path()).await?;
        let expected = aggregate_hash(&files);
        files.reverse();

        assert_eq!(aggregate_hash(&files), expected);
        Ok(())
    }

    #[tokio::test]
    async fn builder_emits_named_manifest_metadata_and_aggregate() -> Result<()> {
        let directory = tempdir()?;
        let mut index = fs::File::create(directory.path().join("index.html"))?;
        index.write_all(b"home")?;
        let files = snapshot_nsite_directory(directory.path()).await?;
        let input = NsiteManifestInput {
            identifier: Some("workshop".to_owned()),
            title: Some("Git Workshop".to_owned()),
            description: Some("Nostr-native collaboration".to_owned()),
            source: Some("nostr://example/repository".to_owned()),
            servers: vec![Url::parse("https://blossom.example")?],
        };

        let (builder, aggregate) = manifest_event_builder(&files, &input)?;
        let unsigned = builder.finalize_unsigned(nostr::prelude::Keys::generate().public_key());
        let tags = unsigned.tags.iter().map(Tag::as_slice).collect::<Vec<_>>();

        assert_eq!(unsigned.kind, NSITE_NAMED_KIND);
        assert!(tags.iter().any(|tag| tag == &["d", "workshop"]));
        assert!(tags.iter().any(|tag| tag == &["title", "Git Workshop"]));
        assert!(
            tags.iter()
                .any(|tag| tag == &["x", aggregate.as_str(), "aggregate"])
        );
        Ok(())
    }

    #[tokio::test]
    async fn builder_emits_root_manifest_and_spec_aggregate() -> Result<()> {
        let directory = tempdir()?;
        fs::File::create(directory.path().join("empty.txt"))?;
        fs::write(directory.path().join("hello.txt"), b"hello")?;
        let files = snapshot_nsite_directory(directory.path()).await?;

        let (builder, aggregate) = manifest_event_builder(&files, &NsiteManifestInput::default())?;
        let unsigned = builder.finalize_unsigned(nostr::prelude::Keys::generate().public_key());

        assert_eq!(unsigned.kind, NSITE_ROOT_KIND);
        assert_eq!(
            aggregate,
            "f2a48d1a81a3dd15cf374f019630dcfdc233b7769574b39b650992635abfdf6c"
        );
        assert!(
            unsigned
                .tags
                .iter()
                .all(|tag| tag.as_slice().first().map(String::as_str) != Some("d"))
        );
        Ok(())
    }

    #[test]
    fn validates_named_site_dns_suffix() {
        for valid in ["a", "gitworkshop", "site-2", "1234567890123"] {
            validate_named_site_identifier(valid).unwrap();
        }
        for invalid in ["", "Upper", "site_2", "site-", "12345678901234"] {
            assert!(
                validate_named_site_identifier(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}
