//! Strict parsing and repository-relative resolution of container manifests.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use nostr::prelude::{RelayUrl, Url};
use serde::{
    Deserialize, Deserializer,
    de::{Error as _, MapAccess, Visitor},
};

use crate::{blossom::canonicalize_blossom_server_root, oci::is_valid_repository_name};

/// Project-relative location discovered when no explicit path is supplied.
pub const DEFAULT_CONTAINER_MANIFEST_PATH: &str = ".ngit/containers.yaml";

/// Stable transport defaults shared by every configured container repository.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerManifestPublication {
    #[serde(default)]
    pub blossom_servers: Vec<String>,
    #[serde(default)]
    pub relays: Vec<String>,
}

/// One container repository's stable project settings.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerManifestEntry {
    pub layout: Option<PathBuf>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub source: Option<String>,
}

/// The on-disk `.ngit/containers.yaml` document.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerManifest {
    pub schema: u32,
    #[serde(default)]
    pub publication: ContainerManifestPublication,
    #[serde(deserialize_with = "deserialize_unique_containers")]
    pub containers: BTreeMap<String, ContainerManifestEntry>,
}

/// A manifest loaded from a repository-relative or absolute path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedContainerManifest {
    pub path: PathBuf,
    pub manifest: ContainerManifest,
}

/// Parse, validate, and normalize a container manifest.
pub fn parse_container_manifest(input: &str) -> Result<ContainerManifest> {
    let mut manifest: ContainerManifest =
        serde_yaml::from_str(input).context("container manifest is not valid YAML")?;
    manifest.validate_and_normalize()?;
    Ok(manifest)
}

/// Resolve an explicit manifest path from the repository root.
///
/// Absolute paths are retained. Relative paths, including the default, are
/// relative to `repository_root`, never the process's current directory.
pub fn resolve_container_manifest_path(
    repository_root: &Path,
    requested_path: Option<&Path>,
) -> Result<PathBuf> {
    let requested_path =
        requested_path.unwrap_or_else(|| Path::new(DEFAULT_CONTAINER_MANIFEST_PATH));
    if requested_path.as_os_str().is_empty() {
        bail!("container manifest path must not be empty");
    }
    Ok(if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        repository_root.join(requested_path)
    })
}

/// Load a container manifest from an explicit path or the default location.
pub fn load_container_manifest(
    repository_root: &Path,
    requested_path: Option<&Path>,
) -> Result<LoadedContainerManifest> {
    let path = resolve_container_manifest_path(repository_root, requested_path)?;
    let input = fs::read_to_string(&path)
        .with_context(|| format!("failed to read container manifest {}", path.display()))?;
    let manifest = parse_container_manifest(&input)
        .with_context(|| format!("invalid container manifest {}", path.display()))?;
    Ok(LoadedContainerManifest { path, manifest })
}

fn deserialize_unique_containers<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, ContainerManifestEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ContainerMapVisitor;

    impl<'de> Visitor<'de> for ContainerMapVisitor {
        type Value = BTreeMap<String, ContainerManifestEntry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of unique container repository names")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut containers = BTreeMap::new();
            while let Some((name, entry)) = map.next_entry::<String, ContainerManifestEntry>()? {
                if containers.insert(name.clone(), entry).is_some() {
                    return Err(A::Error::custom(format!(
                        "duplicate container repository {name:?}"
                    )));
                }
            }
            Ok(containers)
        }
    }

    deserializer.deserialize_map(ContainerMapVisitor)
}

impl ContainerManifest {
    fn validate_and_normalize(&mut self) -> Result<()> {
        if self.schema != 1 {
            bail!(
                "unsupported container manifest schema {}; expected 1",
                self.schema
            );
        }
        if self.containers.is_empty() {
            bail!("container manifest must define at least one container");
        }

        normalize_servers(&mut self.publication.blossom_servers)?;
        normalize_relays(&mut self.publication.relays)?;

        for (name, entry) in &self.containers {
            if !is_valid_repository_name(name) {
                bail!(
                    "container manifest repository name {name:?} is invalid; use lowercase letters, digits, and . _ - separators"
                );
            }
            entry
                .validate()
                .with_context(|| format!("invalid containers.{name}"))?;
        }
        Ok(())
    }
}

impl ContainerManifestEntry {
    fn validate(&self) -> Result<()> {
        if self
            .layout
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            bail!("layout path must not be empty");
        }
        validate_optional_text("title", self.title.as_deref())?;
        validate_optional_text("description", self.description.as_deref())?;
        if let Some(source) = self.source.as_deref() {
            let source = Url::parse(source).context("source is not a valid URL")?;
            if !matches!(source.scheme(), "http" | "https") || source.host().is_none() {
                bail!("source must be an absolute HTTP or HTTPS URL");
            }
            if !source.username().is_empty() || source.password().is_some() {
                bail!("source must not contain embedded credentials");
            }
        }
        Ok(())
    }
}

fn validate_optional_text(field: &str, value: Option<&str>) -> Result<()> {
    if value.is_some_and(str::is_empty) {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn normalize_servers(servers: &mut Vec<String>) -> Result<()> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(servers.len());
    for server in servers.iter() {
        let server = canonicalize_blossom_server_root(server)
            .with_context(|| format!("invalid publication.blossom_servers entry {server:?}"))?
            .to_string();
        if seen.insert(server.clone()) {
            normalized.push(server);
        }
    }
    *servers = normalized;
    Ok(())
}

fn normalize_relays(relays: &mut Vec<String>) -> Result<()> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(relays.len());
    for relay in relays.iter() {
        let relay = RelayUrl::parse(relay)
            .with_context(|| format!("invalid publication.relays entry {relay:?}"))?
            .to_string();
        let key = relay.trim_end_matches('/').to_owned();
        if seen.insert(key) {
            normalized.push(relay);
        }
    }
    *relays = normalized;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    const COMPLETE: &str = r#"
schema: 1
publication:
  blossom_servers:
    - https://blossom.example
    - https://blossom.example/
  relays:
    - wss://relay.example
containers:
  api:
    layout: artifacts/api
    title: Example API
    description: Published from CI
    source: https://example.com/api
  worker:
    layout: /tmp/worker-layout
"#;

    #[test]
    fn parses_and_normalizes_multiple_containers() {
        let manifest = parse_container_manifest(COMPLETE).unwrap();
        assert_eq!(manifest.schema, 1);
        assert_eq!(manifest.containers.len(), 2);
        assert_eq!(
            manifest.publication.blossom_servers,
            ["https://blossom.example/"]
        );
        assert_eq!(manifest.publication.relays, ["wss://relay.example"]);
        assert_eq!(
            manifest.containers["api"].layout.as_deref(),
            Some(Path::new("artifacts/api"))
        );
    }

    #[test]
    fn rejects_unknown_schema_keys_and_invalid_entries() {
        for input in [
            "schema: 2\ncontainers:\n  api:\n    layout: dist\n",
            "schema: 1\nunknown: true\ncontainers:\n  api:\n    layout: dist\n",
            "schema: 1\ncontainers:\n  Bad/Name:\n    layout: dist\n",
            "schema: 1\ncontainers:\n  api:\n    source: file:///tmp/image\n",
            "schema: 1\npublication:\n  blossom_server: https://example.com\ncontainers:\n  api:\n    layout: dist\n",
        ] {
            assert!(
                parse_container_manifest(input).is_err(),
                "accepted:\n{input}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_yaml_fields() {
        for input in [
            "schema: 1\nschema: 1\ncontainers:\n  api:\n    layout: dist\n",
            "schema: 1\ncontainers:\n  api:\n    layout: one\n  api:\n    layout: two\n",
        ] {
            let error = parse_container_manifest(input).unwrap_err();
            assert!(format!("{error:#}").contains("duplicate"));
        }
    }

    #[test]
    fn resolves_default_relative_and_absolute_paths() {
        let root = Path::new("/repo");
        assert_eq!(
            resolve_container_manifest_path(root, None).unwrap(),
            Path::new("/repo/.ngit/containers.yaml")
        );
        assert_eq!(
            resolve_container_manifest_path(root, Some(Path::new("containers.yaml"))).unwrap(),
            Path::new("/repo/containers.yaml")
        );
        assert_eq!(
            resolve_container_manifest_path(root, Some(Path::new("/tmp/containers.yml"))).unwrap(),
            Path::new("/tmp/containers.yml")
        );
    }

    #[test]
    fn loads_from_an_explicit_repository_relative_path() {
        let root = tempdir().unwrap();
        let path = root.path().join("config/containers.yml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, COMPLETE).unwrap();

        let loaded =
            load_container_manifest(root.path(), Some(Path::new("config/containers.yml"))).unwrap();
        assert_eq!(loaded.path, path);
        assert!(loaded.manifest.containers.contains_key("api"));
    }
}
