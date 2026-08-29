//! OCI image-layout validation and Nostr container repository events.
//!
//! Container repositories are addressable kind-30624 events. The event only
//! maps mutable image tags to manifest digests; the manifests, configs, and
//! layers remain ordinary SHA-256-addressed OCI blobs stored on Blossom.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use bitcoin_hashes::{HashEngine as _, sha256};
use nostr::prelude::{Coordinate, Event, EventBuilder, Kind, Tag, Url};
use serde::{Deserialize, Serialize};

pub const CONTAINER_REPOSITORY_KIND: Kind = Kind::Custom(30_624);
pub const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_IMAGE_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

const OCI_LAYOUT_VERSION: &str = "1.0.0";
const OCI_REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MANIFEST_DEPTH: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContainerTag {
    pub name: String,
    /// Bare, lowercase SHA-256 as used by Blossom and kind 30624.
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciBlob {
    pub path: PathBuf,
    /// Bare, lowercase SHA-256 as used by Blossom.
    pub digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciLayout {
    pub tags: Vec<ContainerTag>,
    /// Every blob reachable from a published tag, ordered by digest.
    pub blobs: Vec<OciBlob>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayoutMarker {
    image_layout_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageIndex {
    schema_version: u64,
    manifests: Vec<Descriptor>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestDocument {
    schema_version: u64,
    media_type: Option<String>,
    #[serde(default)]
    manifests: Vec<Descriptor>,
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

impl OciLayout {
    /// Load an OCI image layout and verify the complete descriptor graph.
    ///
    /// Only blobs reachable from a tagged root descriptor are returned. Each
    /// file's size and SHA-256 are checked before publication can begin.
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.is_dir(),
            "OCI layout must be a directory: {}",
            path.display()
        );

        let marker_path = path.join("oci-layout");
        let marker: LayoutMarker = read_json_file(&marker_path, MAX_MANIFEST_BYTES)
            .context("failed to read the OCI layout marker")?;
        ensure!(
            marker.image_layout_version == OCI_LAYOUT_VERSION,
            "unsupported OCI image layout version {:?}; expected {OCI_LAYOUT_VERSION:?}",
            marker.image_layout_version
        );

        let index_path = path.join("index.json");
        let index: ImageIndex = read_json_file(&index_path, MAX_MANIFEST_BYTES)
            .context("failed to read the OCI layout index")?;
        ensure!(
            index.schema_version == 2,
            "OCI layout index schemaVersion must be 2"
        );

        let mut tags = Vec::new();
        let mut tag_digests = BTreeMap::<String, String>::new();
        let mut roots = Vec::new();
        for descriptor in index.manifests {
            let Some(name) = descriptor.annotations.get(OCI_REF_NAME_ANNOTATION) else {
                continue;
            };
            ensure!(
                is_valid_container_tag(name),
                "OCI reference annotation {name:?} is not a valid container tag"
            );
            let digest = parse_sha256_digest(&descriptor.digest).with_context(|| {
                format!("container tag {name:?} has an invalid manifest digest")
            })?;
            if let Some(existing) = tag_digests.get(name) {
                ensure!(
                    existing == &digest,
                    "OCI layout maps container tag {name:?} to more than one digest"
                );
                continue;
            }
            ensure!(
                is_manifest_media_type(&descriptor.media_type),
                "container tag {name:?} points to unsupported manifest media type {:?}",
                descriptor.media_type
            );
            tag_digests.insert(name.clone(), digest.clone());
            tags.push(ContainerTag {
                name: name.clone(),
                digest,
            });
            roots.push(descriptor);
        }
        ensure!(
            !tags.is_empty(),
            "OCI layout index contains no tagged manifests; build the layout with an image tag or add the {OCI_REF_NAME_ANNOTATION:?} annotation"
        );

        let mut blobs = BTreeMap::new();
        let mut parsed_manifests = BTreeMap::new();
        for root in roots {
            visit_manifest(path, &root, 0, true, &mut parsed_manifests, &mut blobs)?;
        }

        Ok(Self {
            tags,
            blobs: blobs.into_values().collect(),
        })
    }
}

fn visit_manifest(
    layout: &Path,
    descriptor: &Descriptor,
    depth: usize,
    repository_root: bool,
    parsed_manifests: &mut BTreeMap<String, String>,
    blobs: &mut BTreeMap<String, OciBlob>,
) -> Result<()> {
    ensure!(
        depth <= MAX_MANIFEST_DEPTH,
        "OCI manifest graph exceeds the supported depth of {MAX_MANIFEST_DEPTH}"
    );
    ensure!(
        is_manifest_media_type(&descriptor.media_type),
        "descriptor uses unsupported manifest media type {:?}",
        descriptor.media_type
    );
    let digest = verify_blob(layout, descriptor, blobs)?;
    if let Some(previous_media_type) = parsed_manifests.get(&digest) {
        ensure!(
            previous_media_type == &descriptor.media_type,
            "manifest {digest} is referenced with conflicting media types {previous_media_type:?} and {:?}",
            descriptor.media_type
        );
        return Ok(());
    }
    parsed_manifests.insert(digest.clone(), descriptor.media_type.clone());
    ensure!(
        descriptor.size <= MAX_MANIFEST_BYTES,
        "manifest {digest} is larger than {MAX_MANIFEST_BYTES} bytes"
    );
    let manifest_path = blobs
        .get(&digest)
        .with_context(|| format!("verified OCI manifest {digest} was not retained"))?
        .path
        .clone();
    let bytes = fs::read(&manifest_path)
        .with_context(|| format!("failed to read OCI manifest {digest}"))?;
    let document: ManifestDocument = serde_json::from_slice(&bytes)
        .with_context(|| format!("manifest {digest} is not valid JSON"))?;
    ensure!(
        document.schema_version == 2,
        "manifest {digest} schemaVersion must be 2"
    );
    if repository_root {
        ensure!(
            document.media_type.is_some(),
            "manifest {digest} declares no mediaType; kind-30624 tags carry only a digest, so tagged manifests must identify their own media type"
        );
    }
    if let Some(embedded) = document.media_type.as_deref() {
        ensure!(
            embedded == descriptor.media_type,
            "manifest {digest} declares mediaType {embedded:?} but its descriptor uses {:?}",
            descriptor.media_type
        );
    }

    if is_index_media_type(&descriptor.media_type) {
        ensure!(
            document.config.is_none() && document.layers.is_empty(),
            "image index {digest} contains image-manifest fields"
        );
        for child in document.manifests {
            visit_manifest(layout, &child, depth + 1, false, parsed_manifests, blobs)?;
        }
    } else {
        ensure!(
            document.manifests.is_empty(),
            "image manifest {digest} contains an index manifest list"
        );
        let config = document
            .config
            .with_context(|| format!("image manifest {digest} has no config descriptor"))?;
        verify_blob(layout, &config, blobs)?;
        for layer in document.layers {
            verify_blob(layout, &layer, blobs)?;
        }
    }
    Ok(())
}

fn verify_blob(
    layout: &Path,
    descriptor: &Descriptor,
    blobs: &mut BTreeMap<String, OciBlob>,
) -> Result<String> {
    ensure!(
        !descriptor.media_type.is_empty(),
        "OCI descriptor has an empty mediaType"
    );
    let digest = parse_sha256_digest(&descriptor.digest)?;
    if let Some(existing) = blobs.get(&digest) {
        ensure!(
            existing.size == descriptor.size,
            "blob {digest} is declared with conflicting sizes"
        );
        return Ok(digest);
    }

    let blob_path = layout.join("blobs").join("sha256").join(&digest);
    let metadata = fs::symlink_metadata(&blob_path)
        .with_context(|| format!("OCI blob {digest} is missing at {}", blob_path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "OCI blob {digest} must be a regular file"
    );
    ensure!(
        metadata.len() == descriptor.size,
        "OCI blob {digest} has size {}, expected {}",
        metadata.len(),
        descriptor.size
    );
    let mut file =
        fs::File::open(&blob_path).with_context(|| format!("failed to read OCI blob {digest}"))?;
    let mut engine = sha256::Hash::engine();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed while hashing OCI blob {digest}"))?;
        if read == 0 {
            break;
        }
        engine.input(&buffer[..read]);
    }
    let observed = sha256::Hash::from_engine(engine).to_string();
    ensure!(observed == digest, "OCI blob {digest} hashes to {observed}");
    blobs.insert(
        digest.clone(),
        OciBlob {
            path: blob_path,
            digest: digest.clone(),
            size: descriptor.size,
        },
    );
    Ok(digest)
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path, max_bytes: u64) -> Result<T> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{} must be a regular file",
        path.display()
    );
    ensure!(
        metadata.len() <= max_bytes,
        "{} is larger than {max_bytes} bytes",
        path.display()
    );
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not valid JSON", path.display()))
}

pub fn parse_sha256_digest(value: &str) -> Result<String> {
    let digest = value
        .strip_prefix("sha256:")
        .with_context(|| format!("digest {value:?} does not use sha256"))?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "digest {value:?} is not a lowercase SHA-256"
    );
    Ok(digest.to_owned())
}

pub fn is_valid_container_tag(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'))
}

pub fn is_valid_repository_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let component_start = index;
        while index < bytes.len() && is_lower_alphanumeric(bytes[index]) {
            index += 1;
        }
        if index == component_start {
            return false;
        }
        if index == bytes.len() {
            return true;
        }
        match bytes[index] {
            b'.' => index += 1,
            b'_' => {
                index += 1;
                if bytes.get(index) == Some(&b'_') {
                    index += 1;
                }
            }
            b'-' => {
                while bytes.get(index) == Some(&b'-') {
                    index += 1;
                }
            }
            _ => return false,
        }
    }
    false
}

fn is_lower_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_digit() || byte.is_ascii_lowercase()
}

fn is_index_media_type(media_type: &str) -> bool {
    matches!(media_type, OCI_IMAGE_INDEX | DOCKER_MANIFEST_LIST)
}

fn is_manifest_media_type(media_type: &str) -> bool {
    is_index_media_type(media_type)
        || matches!(media_type, OCI_IMAGE_MANIFEST | DOCKER_IMAGE_MANIFEST)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerRepository {
    pub name: String,
    /// NIP-34 Git repository this container repository belongs to.
    pub repository: Coordinate,
    pub tags: Vec<ContainerTag>,
    pub servers: Vec<Url>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub source: Option<Url>,
    /// Tags unknown to this ngit version, retained across ordinary updates.
    pub extra_tags: Vec<Tag>,
}

impl ContainerRepository {
    pub fn from_event(event: &Event) -> Result<Self> {
        ensure!(
            event.kind == CONTAINER_REPOSITORY_KIND,
            "event is not a container repository"
        );
        let mut name = None;
        for tag in event.tags.iter().filter(|tag| {
            tag.as_slice()
                .first()
                .is_some_and(|tag_name| tag_name == "d")
        }) {
            ensure!(
                name.is_none(),
                "container repository event has more than one d tag"
            );
            let value = tag
                .as_slice()
                .get(1)
                .context("container repository event has an invalid d tag")?;
            name = Some(value.clone());
        }
        let name = name.context("container repository event has no d tag")?;
        ensure!(
            is_valid_repository_name(&name),
            "container repository event has an invalid name {name:?}"
        );

        let mut tag_map = BTreeMap::new();
        let mut repository = None;
        let mut repository_tag_seen = false;
        let mut servers = Vec::new();
        let mut server_keys = HashSet::new();
        let mut title = None;
        let mut description = None;
        let mut source = None;
        let mut extra_tags = Vec::new();
        for tag in event.tags.iter() {
            match tag.as_slice() {
                [tag_name, ..] if tag_name == "a" => {
                    ensure!(
                        !repository_tag_seen,
                        "container repository event has more than one a tag"
                    );
                    repository_tag_seen = true;
                    let value = tag
                        .as_slice()
                        .get(1)
                        .context("container repository event has an invalid a tag")?;
                    let coordinate = Coordinate::parse(value).with_context(|| {
                        format!("container repository event has an invalid a tag {value:?}")
                    })?;
                    repository = Some(coordinate);
                }
                [tag_name, value, digest, ..]
                    if tag_name == "tag"
                        && is_valid_container_tag(value)
                        && digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
                {
                    tag_map.insert(value.clone(), digest.clone());
                }
                [tag_name, value, ..] if tag_name == "server" => {
                    if let Ok(server) = Url::parse(value) {
                        if matches!(server.scheme(), "http" | "https")
                            && server.host().is_some()
                            && server.username().is_empty()
                            && server.password().is_none()
                            && matches!(server.path(), "" | "/")
                            && server.query().is_none()
                            && server.fragment().is_none()
                            && server_keys.insert(server.to_string())
                        {
                            servers.push(server);
                        }
                    }
                }
                [tag_name, value, ..] if tag_name == "title" && title.is_none() => {
                    title = (!value.is_empty()).then(|| value.clone());
                }
                [tag_name, value, ..] if tag_name == "description" && description.is_none() => {
                    description = (!value.is_empty()).then(|| value.clone());
                }
                [tag_name, value, ..] if tag_name == "source" && source.is_none() => {
                    source = Url::parse(value).ok().filter(|url| {
                        matches!(url.scheme(), "http" | "https")
                            && url.host().is_some()
                            && url.username().is_empty()
                            && url.password().is_none()
                    });
                }
                [tag_name, ..] if is_known_repository_tag(tag_name) => {}
                _ => extra_tags.push(tag.clone()),
            }
        }
        let repository = repository.context("container repository event has no a tag")?;
        ensure!(
            repository.kind == Kind::GitRepoAnnouncement,
            "container repository a tag is not a Nostr Git repository coordinate"
        );

        Ok(Self {
            name,
            repository,
            tags: tag_map
                .into_iter()
                .map(|(name, digest)| ContainerTag { name, digest })
                .collect(),
            servers,
            title,
            description,
            source,
            extra_tags,
        })
    }

    pub fn event_builder(&self) -> Result<EventBuilder> {
        ensure!(
            is_valid_repository_name(&self.name),
            "container repository name {:?} is invalid; use lowercase letters, digits, and . _ - separators",
            self.name
        );
        ensure!(!self.tags.is_empty(), "container repository has no tags");
        ensure!(
            !self.servers.is_empty(),
            "container repository has no Blossom servers"
        );
        ensure!(
            self.repository.kind == Kind::GitRepoAnnouncement,
            "container repository must reference a Nostr Git repository"
        );

        let mut tags = vec![
            Tag::identifier(&self.name),
            Tag::coordinate(self.repository.clone(), None),
        ];
        let mut names = HashSet::new();
        for entry in &self.tags {
            ensure!(
                is_valid_container_tag(&entry.name),
                "container tag {:?} is invalid",
                entry.name
            );
            ensure!(
                names.insert(&entry.name),
                "container tag {:?} is declared more than once",
                entry.name
            );
            ensure!(
                entry.digest.len() == 64
                    && entry
                        .digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "container tag {:?} has an invalid SHA-256",
                entry.name
            );
            tags.push(Tag::parse(["tag", &entry.name, &entry.digest])?);
        }
        for server in &self.servers {
            ensure!(
                matches!(server.scheme(), "http" | "https")
                    && server.host().is_some()
                    && server.username().is_empty()
                    && server.password().is_none()
                    && matches!(server.path(), "" | "/")
                    && server.query().is_none()
                    && server.fragment().is_none(),
                "Blossom server must be an absolute HTTP or HTTPS server root"
            );
            tags.push(Tag::parse(["server", server.as_str()])?);
        }
        if let Some(title) = self.title.as_deref() {
            ensure!(
                !title.is_empty(),
                "container repository title must not be empty"
            );
            tags.push(Tag::parse(["title", title])?);
        }
        if let Some(description) = self.description.as_deref() {
            ensure!(
                !description.is_empty(),
                "container repository description must not be empty"
            );
            tags.push(Tag::parse(["description", description])?);
        }
        if let Some(source) = self.source.as_ref() {
            ensure!(
                matches!(source.scheme(), "http" | "https")
                    && source.host().is_some()
                    && source.username().is_empty()
                    && source.password().is_none(),
                "container repository source must be an absolute HTTP or HTTPS URL"
            );
            tags.push(Tag::parse(["source", source.as_str()])?);
        }
        for tag in &self.extra_tags {
            let name = tag.as_slice().first().map(String::as_str).unwrap_or("");
            ensure!(
                !is_known_repository_tag(name),
                "extra container repository tags must not use known tag name {name:?}"
            );
            tags.push(tag.clone());
        }
        Ok(EventBuilder::new(CONTAINER_REPOSITORY_KIND, "").tags(tags))
    }
}

fn is_known_repository_tag(name: &str) -> bool {
    matches!(
        name,
        "d" | "a" | "tag" | "server" | "title" | "description" | "source"
    )
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{
        Keys,
        event::{FinalizeUnsignedEvent, SignEvent},
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn write_blob(layout: &Path, value: &serde_json::Value) -> Result<(String, u64)> {
        let bytes = serde_json::to_vec(value)?;
        let digest = sha256::Hash::hash(&bytes).to_string();
        fs::write(layout.join("blobs/sha256").join(&digest), &bytes)?;
        Ok((digest, bytes.len() as u64))
    }

    fn descriptor(media_type: &str, digest: &str, size: u64) -> serde_json::Value {
        json!({
            "mediaType": media_type,
            "digest": format!("sha256:{digest}"),
            "size": size,
        })
    }

    fn valid_layout() -> Result<(tempfile::TempDir, String, String)> {
        let directory = tempdir()?;
        let layout = directory.path();
        fs::create_dir_all(layout.join("blobs/sha256"))?;
        fs::write(
            layout.join("oci-layout"),
            serde_json::to_vec(&json!({"imageLayoutVersion": "1.0.0"}))?,
        )?;

        let config_bytes = b"{}";
        let config_digest = sha256::Hash::hash(config_bytes).to_string();
        fs::write(
            layout.join("blobs/sha256").join(&config_digest),
            config_bytes,
        )?;
        let layer_bytes = b"container layer";
        let layer_digest = sha256::Hash::hash(layer_bytes).to_string();
        fs::write(layout.join("blobs/sha256").join(&layer_digest), layer_bytes)?;
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_MANIFEST,
            "config": descriptor("application/vnd.oci.image.config.v1+json", &config_digest, config_bytes.len() as u64),
            "layers": [descriptor("application/vnd.oci.image.layer.v1.tar", &layer_digest, layer_bytes.len() as u64)],
        });
        let (manifest_digest, manifest_size) = write_blob(layout, &manifest)?;
        fs::write(
            layout.join("index.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "manifests": [{
                    "mediaType": OCI_IMAGE_MANIFEST,
                    "digest": format!("sha256:{manifest_digest}"),
                    "size": manifest_size,
                    "annotations": {OCI_REF_NAME_ANNOTATION: "latest"},
                }],
            }))?,
        )?;
        Ok((directory, manifest_digest, layer_digest))
    }

    #[test]
    fn loads_and_verifies_reachable_layout_graph() -> Result<()> {
        let (directory, manifest_digest, layer_digest) = valid_layout()?;
        let layout = OciLayout::load(directory.path())?;

        assert_eq!(
            layout.tags,
            [ContainerTag {
                name: "latest".to_owned(),
                digest: manifest_digest.clone(),
            }]
        );
        assert_eq!(layout.blobs.len(), 3);
        assert!(
            layout
                .blobs
                .iter()
                .any(|blob| blob.digest == manifest_digest)
        );
        assert!(layout.blobs.iter().any(|blob| blob.digest == layer_digest));
        Ok(())
    }

    #[test]
    fn rejects_a_blob_whose_bytes_do_not_match_its_name() -> Result<()> {
        let (directory, manifest_digest, _) = valid_layout()?;
        fs::write(
            directory.path().join("blobs/sha256").join(manifest_digest),
            b"tampered",
        )?;

        let error = OciLayout::load(directory.path()).unwrap_err();
        assert!(
            error.to_string().contains("has size") || error.to_string().contains("hashes to"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn rejects_a_manifest_digest_referenced_with_conflicting_media_types() -> Result<()> {
        let (directory, manifest_digest, _) = valid_layout()?;
        let manifest_size =
            fs::metadata(directory.path().join("blobs/sha256").join(&manifest_digest))?.len();
        fs::write(
            directory.path().join("index.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "manifests": [
                    {
                        "mediaType": OCI_IMAGE_MANIFEST,
                        "digest": format!("sha256:{manifest_digest}"),
                        "size": manifest_size,
                        "annotations": {OCI_REF_NAME_ANNOTATION: "latest"},
                    },
                    {
                        "mediaType": OCI_IMAGE_INDEX,
                        "digest": format!("sha256:{manifest_digest}"),
                        "size": manifest_size,
                        "annotations": {OCI_REF_NAME_ANNOTATION: "edge"},
                    },
                ],
            }))?,
        )?;

        let error = OciLayout::load(directory.path()).unwrap_err();
        assert!(
            error.to_string().contains("conflicting media types"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn repository_and_tag_grammars_match_the_distribution_spec() {
        for valid in ["app", "my.app", "my_app", "my__app", "my---app"] {
            assert!(is_valid_repository_name(valid), "{valid}");
        }
        for invalid in ["", "MyApp", "-app", "app-", "my..app", "my___app"] {
            assert!(!is_valid_repository_name(invalid), "{invalid}");
        }
        for valid in ["latest", "1.2.3", "_internal", "RC-1"] {
            assert!(is_valid_container_tag(valid), "{valid}");
        }
        for invalid in ["", ".latest", "bad/tag", &"x".repeat(129)] {
            assert!(!is_valid_container_tag(invalid), "{invalid}");
        }
    }

    #[test]
    fn builds_the_minimal_container_repository_event() -> Result<()> {
        let keys = Keys::generate();
        let git_repository = Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key())
            .identifier("source-repository");
        let repository = ContainerRepository {
            name: "my-app".to_owned(),
            repository: git_repository.clone(),
            tags: vec![ContainerTag {
                name: "latest".to_owned(),
                digest: "a".repeat(64),
            }],
            servers: vec![Url::parse("https://blossom.example/")?],
            title: Some("My App".to_owned()),
            description: Some("A tiny container".to_owned()),
            source: Some(Url::parse("https://example.com/source")?),
            extra_tags: vec![],
        };
        let event = keys.sign_event(
            repository
                .event_builder()?
                .finalize_unsigned(keys.public_key()),
        )?;

        assert_eq!(event.kind, CONTAINER_REPOSITORY_KIND);
        assert!(event.content.is_empty());
        let tags = event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .collect::<Vec<_>>();
        assert!(tags.iter().any(|tag| tag == &["d", "my-app"]));
        assert!(
            tags.iter()
                .any(|tag| tag == &["a", &git_repository.to_string()])
        );
        assert!(
            tags.iter()
                .any(|tag| tag == &["tag", "latest", &"a".repeat(64)])
        );
        assert!(
            tags.iter()
                .any(|tag| tag == &["server", "https://blossom.example/"])
        );
        Ok(())
    }

    #[test]
    fn parses_repository_state_and_preserves_unknown_tags() -> Result<()> {
        let keys = Keys::generate();
        let event = keys.sign_event(
            EventBuilder::new(CONTAINER_REPOSITORY_KIND, "")
                .tags([
                    Tag::identifier("app"),
                    Tag::coordinate(
                        Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key())
                            .identifier("source-repository"),
                        None,
                    ),
                    Tag::parse(["tag", "latest", &"a".repeat(64)])?,
                    Tag::parse(["tag", "latest", &"b".repeat(64)])?,
                    Tag::parse(["server", "https://blossom.example/"])?,
                    Tag::parse(["future", "kept", "verbatim"])?,
                ])
                .finalize_unsigned(keys.public_key()),
        )?;

        let repository = ContainerRepository::from_event(&event)?;
        assert_eq!(repository.tags[0].digest, "b".repeat(64));
        assert_eq!(
            repository.extra_tags[0].as_slice(),
            ["future", "kept", "verbatim"]
        );
        assert!(repository.event_builder().is_ok());
        Ok(())
    }

    #[test]
    fn requires_exactly_one_git_repository_coordinate() -> Result<()> {
        let keys = Keys::generate();
        let required_tags = || -> Result<Vec<Tag>> {
            Ok(vec![
                Tag::identifier("app"),
                Tag::parse(["tag", "latest", &"a".repeat(64)])?,
                Tag::parse(["server", "https://blossom.example/"])?,
            ])
        };
        let signed_event = |tags: Vec<Tag>| {
            keys.sign_event(
                EventBuilder::new(CONTAINER_REPOSITORY_KIND, "")
                    .tags(tags)
                    .finalize_unsigned(keys.public_key()),
            )
        };

        let missing = signed_event(required_tags()?)?;
        assert!(
            ContainerRepository::from_event(&missing)
                .unwrap_err()
                .to_string()
                .contains("no a tag")
        );

        let mut malformed_tags = required_tags()?;
        malformed_tags.push(Tag::parse(["a"])?);
        let malformed = signed_event(malformed_tags)?;
        assert!(
            ContainerRepository::from_event(&malformed)
                .unwrap_err()
                .to_string()
                .contains("invalid a tag")
        );

        let mut wrong_kind_tags = required_tags()?;
        wrong_kind_tags.push(Tag::coordinate(
            Coordinate::new(Kind::TextNote, keys.public_key()).identifier("source-repository"),
            None,
        ));
        let wrong_kind = signed_event(wrong_kind_tags)?;
        assert!(
            ContainerRepository::from_event(&wrong_kind)
                .unwrap_err()
                .to_string()
                .contains("not a Nostr Git repository coordinate")
        );

        let git_repository = Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key())
            .identifier("source-repository");
        let mut duplicate_tags = required_tags()?;
        duplicate_tags.extend([
            Tag::coordinate(git_repository.clone(), None),
            Tag::coordinate(git_repository, None),
        ]);
        let duplicate = signed_event(duplicate_tags)?;
        assert!(
            ContainerRepository::from_event(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("more than one a tag")
        );
        Ok(())
    }

    #[test]
    fn requires_exactly_one_container_repository_name() -> Result<()> {
        let keys = Keys::generate();
        let common_tags = || -> Result<Vec<Tag>> {
            Ok(vec![
                Tag::coordinate(
                    Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key())
                        .identifier("source-repository"),
                    None,
                ),
                Tag::parse(["tag", "latest", &"a".repeat(64)])?,
                Tag::parse(["server", "https://blossom.example/"])?,
            ])
        };
        let signed_event = |tags: Vec<Tag>| {
            keys.sign_event(
                EventBuilder::new(CONTAINER_REPOSITORY_KIND, "")
                    .tags(tags)
                    .finalize_unsigned(keys.public_key()),
            )
        };

        let mut malformed_tags = common_tags()?;
        malformed_tags.push(Tag::parse(["d"])?);
        let malformed = signed_event(malformed_tags)?;
        assert!(
            ContainerRepository::from_event(&malformed)
                .unwrap_err()
                .to_string()
                .contains("invalid d tag")
        );

        let mut duplicate_tags = common_tags()?;
        duplicate_tags.extend([Tag::identifier("app"), Tag::identifier("other-app")]);
        let duplicate = signed_event(duplicate_tags)?;
        assert!(
            ContainerRepository::from_event(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("more than one d tag")
        );
        Ok(())
    }
}
