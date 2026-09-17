//! NIP-5A static-site manifests and immutable directory snapshots.

use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin_hashes::{HashEngine as _, sha256};
use nostr::prelude::{Event, EventBuilder, Kind, RelayUrl, Tag, Url};
use serde::Deserialize;

use crate::{
    blossom::{FileSnapshot, LocalFileRequest, snapshot_local_file},
    release_download::mime_type_from_filename,
};

pub const NSITE_ROOT_KIND: Kind = Kind::Custom(15_128);
pub const NSITE_NAMED_KIND: Kind = Kind::Custom(35_128);
pub const MAX_NAMED_SITE_IDENTIFIER_BYTES: usize = 13;
pub const DEFAULT_NSITE_CONFIG_PATH: &str = ".nsite/config.json";
const MAX_NSITE_CONFIG_BYTES: u64 = 1024 * 1024;

/// The publication fields ngit consumes from nsyte's `.nsite/config.json`.
///
/// Unknown fields remain accepted so projects can keep profile, list, and
/// application-handler settings for nsyte while using ngit for the site
/// publication itself.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NsiteProjectConfig {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub fallback: Option<String>,
    #[serde(default)]
    pub servers: Vec<String>,
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub publish_profile: bool,
    #[serde(default)]
    pub publish_relay_list: bool,
    #[serde(default)]
    pub publish_server_list: bool,
    #[serde(default)]
    pub publish_app_handler: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedNsiteProjectConfig {
    pub path: PathBuf,
    pub config: NsiteProjectConfig,
}

/// One stable local file and the absolute path published for it.
#[derive(Debug)]
pub struct NsiteFileSnapshot {
    pub path: String,
    pub snapshot: Arc<FileSnapshot>,
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
    pub relays: Vec<RelayUrl>,
}

/// Load nsyte-compatible publication defaults.
///
/// An explicit path is required to exist. Without one, the conventional
/// `.nsite/config.json` is loaded only when present in the current directory.
/// YAML is deliberately unsupported because nsyte's project format is JSON.
pub fn load_nsite_project_config(
    explicit_path: Option<&Path>,
    disabled: bool,
) -> Result<Option<LoadedNsiteProjectConfig>> {
    if disabled {
        return Ok(None);
    }
    let (path, required) = explicit_path.map_or_else(
        || (PathBuf::from(DEFAULT_NSITE_CONFIG_PATH), false),
        |path| (path.to_path_buf(), true),
    );
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect nsite config {}", path.display()));
        }
    };
    if !metadata.is_file() || metadata.len() > MAX_NSITE_CONFIG_BYTES {
        bail!(
            "nsite config must be a regular file no larger than {MAX_NSITE_CONFIG_BYTES} bytes: {}",
            path.display()
        );
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read nsite config {}", path.display()))?;
    let config = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse nsite config {} as JSON", path.display()))?;
    Ok(Some(LoadedNsiteProjectConfig { path, config }))
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
        let mut request = LocalFileRequest::new(&source_path);
        request.mime_type = mime_type_from_filename(&public_path)
            .or_else(|| mime_guess::from_path(&public_path).first_raw())
            .map(str::to_owned);
        let snapshot = snapshot_local_file(request)
            .await
            .with_context(|| format!("failed to snapshot nsite path {public_path}"))?;
        snapshots.push(NsiteFileSnapshot {
            path: public_path,
            snapshot: Arc::new(snapshot),
        });
    }
    Ok(snapshots)
}

/// Return one representative snapshot for each content hash.
///
/// Blossom stores MIME metadata against the content hash, so the same bytes
/// cannot safely back paths which require different MIME types.
pub fn unique_blob_snapshots(files: &[NsiteFileSnapshot]) -> Result<Vec<&FileSnapshot>> {
    let mut seen = HashMap::new();
    let mut unique = Vec::new();
    for file in files {
        if let Some((existing_path, existing_mime)) = seen.get(file.snapshot.sha256.as_str()) {
            if *existing_mime != file.snapshot.mime_type {
                bail!(
                    "nsite paths {existing_path} and {} contain identical bytes but require different MIME types ({existing_mime} and {})",
                    file.path,
                    file.snapshot.mime_type
                );
            }
        } else {
            seen.insert(
                file.snapshot.sha256.as_str(),
                (file.path.as_str(), file.snapshot.mime_type.as_str()),
            );
            unique.push(file.snapshot.as_ref());
        }
    }
    Ok(unique)
}

/// Add or replace `/404.html` with the configured fallback file's immutable
/// snapshot, matching nsyte's manifest behavior without copying bytes again.
pub fn apply_nsite_fallback(files: &mut Vec<NsiteFileSnapshot>, fallback: &str) -> Result<()> {
    let relative = fallback.trim_start_matches('/');
    let fallback_path = public_site_path(Path::new(relative))?;
    let fallback_file = files
        .iter()
        .find(|file| file.path == fallback_path)
        .with_context(|| {
            format!("nsite fallback {fallback:?} is not present in the build output")
        })?;
    if fallback_file.snapshot.mime_type != "text/html" {
        bail!(
            "nsite fallback {fallback:?} must resolve to an HTML file, but {fallback_path} has MIME type {}",
            fallback_file.snapshot.mime_type
        );
    }
    let fallback_snapshot = Arc::clone(&fallback_file.snapshot);

    if let Some(not_found) = files.iter_mut().find(|file| file.path == "/404.html") {
        not_found.snapshot = fallback_snapshot;
    } else {
        files.push(NsiteFileSnapshot {
            path: "/404.html".to_owned(),
            snapshot: fallback_snapshot,
        });
    }
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(())
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

    let mut tags = Vec::with_capacity(files.len() + input.servers.len() + input.relays.len() + 6);
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
    for relay in &input.relays {
        tags.push(Tag::parse(["relay", relay.as_str()]).context("invalid nsite relay tag")?);
    }
    append_optional_tag(&mut tags, "title", input.title.as_deref())?;
    append_optional_tag(&mut tags, "description", input.description.as_deref())?;
    append_optional_tag(&mut tags, "source", input.source.as_deref())?;

    Ok((EventBuilder::new(kind, "").tags(tags), aggregate))
}

/// Return whether an existing event already carries the desired manifest.
///
/// Replacement metadata (`created_at`, ID, signature, and our nonce) is
/// deliberately ignored. The builder is deterministic, so equality of kind,
/// tags, and content means publishing another replaceable event would be a
/// no-op.
pub fn event_matches_manifest(event: &Event, builder: &EventBuilder) -> bool {
    event.kind == builder.kind
        && event.content == builder.content
        && event
            .tags
            .iter()
            .filter(|tag| !crate::event_ordering::is_ngit_nonce(tag))
            .eq(builder
                .tags
                .iter()
                .filter(|tag| !crate::event_ordering::is_ngit_nonce(tag)))
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
                if value.is_empty()
                    || value.contains(['/', '\\', '?', '#', '%'])
                    || value.chars().any(char::is_control)
                {
                    bail!("nsite path contains an unsafe component");
                }
                parts.push(value.to_owned());
            }
            _ => bail!("nsite path contains a non-normal component"),
        }
    }
    if parts.is_empty() {
        bail!("nsite file path must not be empty");
    }
    let filename = relative
        .file_name()
        .and_then(|value| value.to_str())
        .context("nsite file path must end with a valid UTF-8 filename")?;
    if Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_none_or(str::is_empty)
    {
        bail!("nsite file paths must end with a filename extension");
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
        if source_url.scheme() == "https"
            && (!source_url.username().is_empty() || source_url.password().is_some())
        {
            bail!("nsite HTTPS source must not contain embedded credentials");
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

    #[tokio::test]
    async fn snapshots_use_the_web_mime_database_beyond_release_formats() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("readme.md"), "hello")?;

        let files = snapshot_nsite_directory(directory.path()).await?;

        assert_eq!(files[0].snapshot.mime_type, "text/markdown");
        assert!(files[0].snapshot.warnings.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn snapshots_prefer_canonical_static_asset_mime_types() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("app.js.map"), "{}")?;
        fs::write(directory.path().join("favicon.ico"), "icon")?;

        let files = snapshot_nsite_directory(directory.path()).await?;

        assert_eq!(files[0].snapshot.mime_type, "application/json");
        assert_eq!(files[1].snapshot.mime_type, "image/vnd.microsoft.icon");
        assert!(files.iter().all(|file| file.snapshot.warnings.is_empty()));
        Ok(())
    }

    #[tokio::test]
    async fn snapshots_retain_a_warning_for_unknown_mime_types() -> Result<()> {
        use crate::release_download::DownloadWarningCode;

        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("data.unknown-ngit-type"), "hello")?;

        let files = snapshot_nsite_directory(directory.path()).await?;

        assert_eq!(files[0].snapshot.mime_type, "application/octet-stream");
        assert!(
            files[0]
                .snapshot
                .warnings
                .iter()
                .any(|warning| warning.code == DownloadWarningCode::GenericMime)
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
    async fn rejects_one_hash_with_conflicting_mime_types() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("app.js"), b"")?;
        fs::write(directory.path().join("style.css"), b"")?;
        let files = snapshot_nsite_directory(directory.path()).await?;

        let error = unique_blob_snapshots(&files).unwrap_err().to_string();

        assert!(error.contains("identical bytes"));
        assert!(error.contains("different MIME types"));
        Ok(())
    }

    #[tokio::test]
    async fn deduplicates_one_hash_with_the_same_mime_type() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("app.js"), b"")?;
        fs::write(directory.path().join("vendor.js"), b"")?;
        let files = snapshot_nsite_directory(directory.path()).await?;

        assert_eq!(unique_blob_snapshots(&files)?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn fallback_adds_404_mapping_without_another_blob() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("index.html"), b"home")?;
        let mut files = snapshot_nsite_directory(directory.path()).await?;

        apply_nsite_fallback(&mut files, "/index.html")?;

        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["/404.html", "/index.html"]
        );
        assert!(Arc::ptr_eq(&files[0].snapshot, &files[1].snapshot));
        assert_eq!(unique_blob_snapshots(&files)?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn fallback_replaces_an_existing_404_mapping() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("404.html"), b"old")?;
        fs::write(directory.path().join("index.html"), b"home")?;
        let mut files = snapshot_nsite_directory(directory.path()).await?;

        apply_nsite_fallback(&mut files, "index.html")?;

        let not_found = files.iter().find(|file| file.path == "/404.html").unwrap();
        let index = files
            .iter()
            .find(|file| file.path == "/index.html")
            .unwrap();
        assert!(Arc::ptr_eq(&not_found.snapshot, &index.snapshot));
        assert_eq!(unique_blob_snapshots(&files)?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn fallback_must_name_a_safe_file_in_the_snapshot() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("index.html"), b"home")?;
        fs::write(directory.path().join("app.js"), b"script")?;
        let mut files = snapshot_nsite_directory(directory.path()).await?;

        assert!(
            apply_nsite_fallback(&mut files, "missing.html")
                .unwrap_err()
                .to_string()
                .contains("not present")
        );
        assert!(apply_nsite_fallback(&mut files, "../index.html").is_err());
        assert!(
            apply_nsite_fallback(&mut files, "app.js")
                .unwrap_err()
                .to_string()
                .contains("must resolve to an HTML file")
        );
        Ok(())
    }

    #[test]
    fn loads_supported_nsyte_config_fields_and_ignores_future_fields() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("config.json");
        fs::write(
            &path,
            r#"{
                "id": "docs",
                "title": "Documentation",
                "description": "Project docs",
                "source": "https://example.com/repo",
                "fallback": "/index.html",
                "servers": ["https://blossom.example"],
                "relays": ["wss://relay.example"],
                "publishProfile": true,
                "profile": {"name": "Alice"}
            }"#,
        )?;

        let loaded = load_nsite_project_config(Some(&path), false)?.unwrap();

        assert_eq!(loaded.path, path);
        assert_eq!(loaded.config.id.as_deref(), Some("docs"));
        assert_eq!(loaded.config.fallback.as_deref(), Some("/index.html"));
        assert_eq!(loaded.config.servers, ["https://blossom.example"]);
        assert_eq!(loaded.config.relays, ["wss://relay.example"]);
        assert!(loaded.config.publish_profile);
        Ok(())
    }

    #[test]
    fn explicit_nsite_config_must_exist_unless_disabled() -> Result<()> {
        let path = Path::new("does-not-exist/config.json");

        assert!(load_nsite_project_config(Some(path), false).is_err());
        assert!(load_nsite_project_config(Some(path), true)?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn rejects_unsafe_paths_and_requires_extensions() -> Result<()> {
        let unsafe_directory = tempdir()?;
        fs::write(unsafe_directory.path().join("bad\nname.html"), b"hello")?;
        let error = snapshot_nsite_directory(unsafe_directory.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsafe component"));

        let extensionless_directory = tempdir()?;
        fs::write(extensionless_directory.path().join("CNAME"), b"example.com")?;
        let error = snapshot_nsite_directory(extensionless_directory.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("filename extension"));
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
            relays: vec![RelayUrl::parse("wss://relay.example")?],
        };

        let (builder, aggregate) = manifest_event_builder(&files, &input)?;
        let unsigned = builder.finalize_unsigned(nostr::prelude::Keys::generate().public_key());
        let tags = unsigned.tags.iter().map(Tag::as_slice).collect::<Vec<_>>();

        assert_eq!(unsigned.kind, NSITE_NAMED_KIND);
        assert!(tags.iter().any(|tag| tag == &["d", "workshop"]));
        assert!(tags.iter().any(|tag| tag == &["title", "Git Workshop"]));
        assert!(
            tags.iter()
                .any(|tag| tag == &["relay", "wss://relay.example"])
        );
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

    #[tokio::test]
    async fn manifest_comparison_ignores_only_replacement_metadata() -> Result<()> {
        use nostr::prelude::{Keys, Timestamp, event::SignEvent as _};

        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("index.html"), "hello")?;
        let files = snapshot_nsite_directory(directory.path()).await?;
        let (builder, _) = manifest_event_builder(&files, &NsiteManifestInput::default())?;
        let keys = Keys::generate();
        let event = keys.sign_event(
            builder
                .clone()
                .tag(Tag::parse(["nonce", "1", "0", "ngit-created-at-tiebreak"])?)
                .custom_created_at(Timestamp::from_secs(1))
                .finalize_unsigned(keys.public_key()),
        )?;

        assert!(event_matches_manifest(&event, &builder));
        assert!(!event_matches_manifest(
            &event,
            &builder
                .clone()
                .tag(Tag::parse(["nonce", "1", "0", "other-tool"])?)
        ));
        let mut changed_content = builder.clone();
        changed_content.content = "changed".to_owned();
        assert!(!event_matches_manifest(&event, &changed_content));
        assert!(!event_matches_manifest(
            &event,
            &builder.tag(Tag::parse(["title", "changed"])?)
        ));
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

    #[test]
    fn rejects_https_source_credentials_without_rejecting_nostr_names() {
        let with_credentials = NsiteManifestInput {
            source: Some("https://user:secret@example.com/repo".to_owned()),
            ..NsiteManifestInput::default()
        };
        assert!(validate_manifest_input(&with_credentials).is_err());

        let nip05_source = NsiteManifestInput {
            source: Some("nostr://dan@example.com/repo".to_owned()),
            ..NsiteManifestInput::default()
        };
        validate_manifest_input(&nip05_source).unwrap();
    }
}
