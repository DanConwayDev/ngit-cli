//! Strict parsing and resolution of `.ngit/release.yaml` manifests.
//!
//! A parsed manifest is intentionally not ready for publication: source and
//! filename templates are resolved only after the release version and optional
//! Git tag are known. Resolution returns a separate type so callers cannot
//! confuse a local file with a URL-backed asset.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use serde::Deserialize;

use crate::apk::APK_MIME_TYPE;

/// Project-relative location used when no explicit manifest path is supplied.
pub const DEFAULT_RELEASE_MANIFEST_PATH: &str = ".ngit/release.yaml";

/// The on-disk release manifest.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub application: Option<String>,
    pub channel: Option<String>,
    pub notes: Option<String>,
    pub release_notes: Option<String>,
    pub commit: Option<String>,
    #[serde(default)]
    pub publication: ReleaseManifestPublication,
    pub assets: Vec<ReleaseManifestAsset>,
}

/// Stable publication and policy defaults for CI release jobs.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifestPublication {
    #[serde(default)]
    pub blossom_servers: Vec<String>,
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub zapstore_relay: bool,
    #[serde(default)]
    pub strict_metadata: bool,
    #[serde(default)]
    pub allow_partial_platforms: bool,
    #[serde(default)]
    pub add_application_platforms: bool,
}

/// One URL- or local-file-backed asset in a release manifest.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifestAsset {
    pub source: Option<String>,
    pub file: Option<String>,
    pub identifier: Option<String>,
    pub version: Option<String>,
    pub filename: Option<String>,
    pub mime: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub platform_agnostic: bool,
    pub min_platform_version: Option<String>,
    pub target_platform_version: Option<String>,
    #[serde(default)]
    pub supported_nips: Vec<String>,
    pub variant: Option<String>,
    pub commit: Option<String>,
    pub min_allowed_version: Option<String>,
    pub android: Option<ReleaseManifestAndroid>,
    pub original_url: Option<String>,
}

/// Android-specific manifest metadata.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifestAndroid {
    pub version_code: Option<u64>,
    pub min_allowed_version_code: Option<u64>,
    #[serde(default)]
    pub certificate_sha256: Vec<String>,
}

/// A manifest loaded from a repository-relative or absolute path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedReleaseManifest {
    pub path: PathBuf,
    pub manifest: ReleaseManifest,
}

/// A manifest whose controlled templates have been expanded and whose sources
/// have been validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedReleaseManifest {
    pub application: Option<String>,
    pub channel: Option<String>,
    pub notes: Option<String>,
    pub release_notes: Option<PathBuf>,
    pub commit: Option<String>,
    pub publication: ReleaseManifestPublication,
    pub assets: Vec<ResolvedReleaseManifestAsset>,
}

/// The unambiguous source of a resolved manifest asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedReleaseManifestSource {
    Url(String),
    File(PathBuf),
}

/// One resolved asset, ready to be passed to source acquisition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedReleaseManifestAsset {
    pub source: ResolvedReleaseManifestSource,
    pub identifier: Option<String>,
    pub version: Option<String>,
    pub filename: Option<String>,
    pub mime: Option<String>,
    pub platforms: Vec<String>,
    pub platform_agnostic: bool,
    pub min_platform_version: Option<String>,
    pub target_platform_version: Option<String>,
    pub supported_nips: Vec<String>,
    pub variant: Option<String>,
    pub commit: Option<String>,
    pub min_allowed_version: Option<String>,
    pub android: Option<ReleaseManifestAndroid>,
    pub original_url: Option<String>,
}

/// Parse and validate a release manifest without resolving its templates.
pub fn parse_release_manifest(input: &str) -> Result<ReleaseManifest> {
    let mut manifest: ReleaseManifest =
        serde_yaml::from_str(input).context("release manifest is not valid YAML")?;
    manifest.validate_and_normalize()?;
    Ok(manifest)
}

/// Resolve an explicit manifest path from the repository root.
///
/// Absolute paths are retained. Relative paths, including the default path,
/// are always relative to `repository_root`, never the caller's current
/// directory.
pub fn resolve_release_manifest_path(
    repository_root: &Path,
    requested_path: Option<&Path>,
) -> Result<PathBuf> {
    let requested_path = requested_path.unwrap_or_else(|| Path::new(DEFAULT_RELEASE_MANIFEST_PATH));
    if requested_path.as_os_str().is_empty() {
        bail!("release manifest path must not be empty");
    }

    Ok(if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        repository_root.join(requested_path)
    })
}

/// Load a release manifest, resolving relative paths from the repository root.
pub fn load_release_manifest(
    repository_root: &Path,
    requested_path: Option<&Path>,
) -> Result<LoadedReleaseManifest> {
    let path = resolve_release_manifest_path(repository_root, requested_path)?;
    let input = fs::read_to_string(&path)
        .with_context(|| format!("failed to read release manifest {}", path.display()))?;
    let manifest = parse_release_manifest(&input)
        .with_context(|| format!("invalid release manifest {}", path.display()))?;
    Ok(LoadedReleaseManifest { path, manifest })
}

impl ReleaseManifest {
    /// Expand the literal `{version}` and `{tag}` placeholders.
    ///
    /// Placeholders are supported in asset source URLs, original URLs, and
    /// filename overrides. URL substitutions are percent-encoded as one URL
    /// component. Filename substitutions are inserted literally and then
    /// subjected to filename validation. Expansion is single-pass: placeholder
    /// text inside a version or tag is data, not another template.
    pub fn resolve(
        &self,
        release_version: &str,
        tag: Option<&str>,
    ) -> Result<ResolvedReleaseManifest> {
        validate_clean_value("release version", release_version)?;
        if let Some(tag) = tag {
            validate_clean_value("release tag", tag)?;
        }

        let mut assets = Vec::with_capacity(self.assets.len());
        let mut source_urls: HashMap<Url, usize> = HashMap::new();
        let mut source_paths: HashMap<PathBuf, usize> = HashMap::new();
        let mut filenames: HashMap<String, usize> = HashMap::new();

        for (index, asset) in self.assets.iter().enumerate() {
            let source = match (&asset.source, &asset.file) {
                (Some(source), None) => {
                    let source = expand_template(
                        source,
                        release_version,
                        tag,
                        TemplateEncoding::UrlComponent,
                    )
                    .with_context(|| format!("invalid template in assets[{index}].source"))?;
                    let source_url = parse_public_url(&source, &format!("assets[{index}].source"))?;
                    if let Some(previous) = source_urls.insert(source_url, index) {
                        bail!(
                            "assets[{index}].source resolves to the same URL as assets[{previous}].source"
                        );
                    }
                    ResolvedReleaseManifestSource::Url(source)
                }
                (None, Some(file)) => {
                    let file = PathBuf::from(
                        expand_template(file, release_version, tag, TemplateEncoding::Literal)
                            .with_context(|| format!("invalid template in assets[{index}].file"))?,
                    );
                    if let Some(previous) = source_paths.insert(file.clone(), index) {
                        bail!(
                            "assets[{index}].file resolves to the same local path as assets[{previous}].file"
                        );
                    }
                    ResolvedReleaseManifestSource::File(file)
                }
                _ => unreachable!("manifest source kind is validated during parsing"),
            };

            let filename = asset
                .filename
                .as_deref()
                .map(|filename| {
                    expand_template(filename, release_version, tag, TemplateEncoding::Literal)
                        .with_context(|| format!("invalid template in assets[{index}].filename"))
                })
                .transpose()?;
            if let Some(filename) = &filename {
                validate_filename(filename)
                    .with_context(|| format!("invalid assets[{index}].filename"))?;
                if let Some(previous) = filenames.insert(filename.clone(), index) {
                    bail!(
                        "assets[{index}].filename resolves to the same filename as assets[{previous}].filename"
                    );
                }
            }

            let original_url = asset
                .original_url
                .as_deref()
                .map(|original_url| {
                    let resolved = expand_template(
                        original_url,
                        release_version,
                        tag,
                        TemplateEncoding::UrlComponent,
                    )
                    .with_context(|| format!("invalid template in assets[{index}].original_url"))?;
                    parse_public_url(&resolved, &format!("assets[{index}].original_url"))?;
                    Ok::<_, anyhow::Error>(resolved)
                })
                .transpose()?;

            assets.push(ResolvedReleaseManifestAsset {
                source,
                identifier: asset.identifier.clone(),
                version: asset.version.clone(),
                filename,
                mime: asset.mime.clone(),
                platforms: asset.platforms.clone(),
                platform_agnostic: asset.platform_agnostic,
                min_platform_version: asset.min_platform_version.clone(),
                target_platform_version: asset.target_platform_version.clone(),
                supported_nips: asset.supported_nips.clone(),
                variant: asset.variant.clone(),
                commit: asset.commit.clone(),
                min_allowed_version: asset.min_allowed_version.clone(),
                android: asset.android.clone(),
                original_url,
            });
        }

        Ok(ResolvedReleaseManifest {
            application: self.application.clone(),
            channel: self.channel.clone(),
            notes: self.notes.clone(),
            release_notes: self.release_notes.as_ref().map(PathBuf::from),
            commit: self.commit.clone(),
            publication: self.publication.clone(),
            assets,
        })
    }

    fn validate_and_normalize(&mut self) -> Result<()> {
        if self.schema != 1 {
            bail!(
                "unsupported release manifest schema {}; expected schema 1",
                self.schema
            );
        }
        validate_optional_clean_value("application", self.application.as_deref())?;
        validate_optional_clean_value("channel", self.channel.as_deref())?;
        validate_optional_clean_value("commit", self.commit.as_deref())?;
        if self
            .notes
            .as_ref()
            .is_some_and(|notes| notes.trim().is_empty())
        {
            bail!("notes must not be empty when supplied");
        }
        validate_optional_clean_value("release_notes", self.release_notes.as_deref())?;
        if self.notes.is_some() && self.release_notes.is_some() {
            bail!("notes and release_notes are mutually exclusive");
        }
        if self.assets.is_empty() {
            bail!("release manifest must contain at least one asset");
        }

        self.publication.validate_and_normalize()?;

        for (index, asset) in self.assets.iter_mut().enumerate() {
            asset
                .validate_and_normalize()
                .with_context(|| format!("invalid assets[{index}]"))?;
        }
        Ok(())
    }
}

/// Extract one version's notes from a Keep a Changelog document.
///
/// The matching level-two heading may use `[VERSION]` or `VERSION`, with an
/// optional leading `v`. Its body ends at the next level-two heading. Markdown
/// inside the body is preserved, apart from surrounding blank lines.
pub fn extract_keep_a_changelog_release_notes(
    changelog: &str,
    release_version: &str,
) -> Result<String> {
    validate_clean_value("release version", release_version)?;
    let release_version = release_version.strip_prefix('v').unwrap_or(release_version);
    let lines = changelog.lines().collect::<Vec<_>>();
    let matches = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let version = keep_a_changelog_heading_version(line)?;
            (version.strip_prefix('v').unwrap_or(version) == release_version).then_some(index)
        })
        .collect::<Vec<_>>();

    let start = match matches.as_slice() {
        [] => bail!(
            "release_notes does not contain a Keep a Changelog section for version {release_version:?}"
        ),
        [start] => start + 1,
        _ => bail!(
            "release_notes contains multiple Keep a Changelog sections for version {release_version:?}"
        ),
    };
    let end = lines[start..]
        .iter()
        .position(|line| is_level_two_heading(line))
        .map_or(lines.len(), |offset| start + offset);
    let section = &lines[start..end];
    let first = section
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or_else(|| {
            anyhow!("Keep a Changelog section for version {release_version:?} is empty")
        })?;
    let last = section
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .expect("a non-empty line was found");
    Ok(section[first..=last].join("\n"))
}

fn keep_a_changelog_heading_version(line: &str) -> Option<&str> {
    let heading = line.strip_prefix("## ")?.trim();
    if let Some(bracketed) = heading.strip_prefix('[') {
        return bracketed.split_once(']').map(|(version, _)| version);
    }
    heading.split_whitespace().next()
}

fn is_level_two_heading(line: &str) -> bool {
    line.strip_prefix("## ")
        .is_some_and(|heading| !heading.trim().is_empty())
}

impl ReleaseManifestPublication {
    fn validate_and_normalize(&mut self) -> Result<()> {
        normalize_string_list("publication.blossom_servers", &mut self.blossom_servers)?;
        normalize_string_list("publication.relays", &mut self.relays)?;
        Ok(())
    }
}

impl ReleaseManifestAsset {
    fn validate_and_normalize(&mut self) -> Result<()> {
        match (&self.source, &self.file) {
            (Some(source), None) => {
                validate_clean_value("source", source)?;
                validate_template(source).context("invalid source template")?;
            }
            (None, Some(file)) => {
                validate_clean_value("file", file)?;
                validate_template(file).context("invalid file template")?;
            }
            (Some(_), Some(_)) => bail!("source and file are mutually exclusive"),
            (None, None) => bail!("provide exactly one of source or file"),
        }
        validate_optional_clean_value("identifier", self.identifier.as_deref())?;
        validate_optional_clean_value("version", self.version.as_deref())?;
        validate_optional_clean_value("filename", self.filename.as_deref())?;
        if let Some(filename) = &self.filename {
            validate_template(filename).context("invalid filename template")?;
        }
        validate_optional_clean_value("mime", self.mime.as_deref())?;
        validate_optional_clean_value(
            "min_platform_version",
            self.min_platform_version.as_deref(),
        )?;
        validate_optional_clean_value(
            "target_platform_version",
            self.target_platform_version.as_deref(),
        )?;
        validate_optional_clean_value("variant", self.variant.as_deref())?;
        validate_optional_clean_value("commit", self.commit.as_deref())?;
        validate_optional_clean_value("min_allowed_version", self.min_allowed_version.as_deref())?;
        validate_optional_clean_value("original_url", self.original_url.as_deref())?;
        if let Some(original_url) = &self.original_url {
            validate_template(original_url).context("invalid original_url template")?;
        }

        normalize_string_list("platforms", &mut self.platforms)?;
        normalize_string_list("supported_nips", &mut self.supported_nips)?;

        let apk = self
            .mime
            .as_deref()
            .is_some_and(|mime| mime.eq_ignore_ascii_case(APK_MIME_TYPE))
            || self
                .file
                .as_deref()
                .is_some_and(|file| file.to_ascii_lowercase().ends_with(".apk"))
            || self
                .filename
                .as_deref()
                .is_some_and(|filename| filename.to_ascii_lowercase().ends_with(".apk"));
        let local_apk = apk && self.file.is_some();

        if self.platform_agnostic && !self.platforms.is_empty() {
            bail!("platforms and platform_agnostic: true are mutually exclusive");
        }
        if self.platform_agnostic && apk {
            bail!("Android APK assets cannot be platform agnostic");
        }
        if !self.platform_agnostic && self.platforms.is_empty() && !local_apk {
            bail!("provide at least one platform or set platform_agnostic: true");
        }

        if let Some(android) = &mut self.android {
            android.validate_and_normalize()?;
        }
        if apk {
            let android = self
                .android
                .as_ref()
                .ok_or_else(|| anyhow!("Android APK assets require an android metadata block"))?;
            if android.version_code.is_none() {
                bail!("Android APK assets require android.version_code");
            }
            if android.certificate_sha256.is_empty() {
                bail!("Android APK assets require android.certificate_sha256");
            }
        }

        Ok(())
    }
}

impl ReleaseManifestAndroid {
    fn validate_and_normalize(&mut self) -> Result<()> {
        if self.version_code.is_none()
            && self.min_allowed_version_code.is_none()
            && self.certificate_sha256.is_empty()
        {
            bail!("android metadata must not be empty");
        }
        if self.version_code == Some(0) {
            bail!("android.version_code must be greater than zero");
        }
        if self.min_allowed_version_code == Some(0) {
            bail!("android.min_allowed_version_code must be greater than zero");
        }
        if self.min_allowed_version_code.is_some() && self.version_code.is_none() {
            bail!("android.min_allowed_version_code requires android.version_code");
        }
        if matches!(
            (self.min_allowed_version_code, self.version_code),
            (Some(minimum), Some(current)) if minimum > current
        ) {
            bail!("android.min_allowed_version_code must not exceed android.version_code");
        }

        normalize_string_list("android.certificate_sha256", &mut self.certificate_sha256)?;
        for hash in &mut self.certificate_sha256 {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("android.certificate_sha256 entries must be 64 hexadecimal characters");
            }
            hash.make_ascii_lowercase();
        }
        // Lowercasing may turn differently-cased inputs into duplicates.
        deduplicate(&mut self.certificate_sha256);
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum TemplateEncoding {
    Literal,
    UrlComponent,
}

fn validate_template(value: &str) -> Result<()> {
    expand_template(value, "version", Some("tag"), TemplateEncoding::Literal).map(|_| ())
}

fn expand_template(
    value: &str,
    version: &str,
    tag: Option<&str>,
    encoding: TemplateEncoding,
) -> Result<String> {
    let encoded_version;
    let encoded_tag;
    let version = match encoding {
        TemplateEncoding::Literal => version,
        TemplateEncoding::UrlComponent => {
            encoded_version = urlencoding::encode(version);
            encoded_version.as_ref()
        }
    };
    let tag = match (tag, encoding) {
        (Some(tag), TemplateEncoding::Literal) => Some(tag),
        (Some(tag), TemplateEncoding::UrlComponent) => {
            encoded_tag = urlencoding::encode(tag);
            Some(encoded_tag.as_ref())
        }
        (None, _) => None,
    };

    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(position) = remaining.find(['{', '}']) {
        output.push_str(&remaining[..position]);
        remaining = &remaining[position..];

        if remaining.starts_with('}') {
            bail!("unexpected closing brace in template");
        }
        let end = remaining
            .find('}')
            .ok_or_else(|| anyhow!("unclosed placeholder in template"))?;
        let placeholder = &remaining[..=end];
        match placeholder {
            "{version}" => output.push_str(version),
            "{tag}" => output
                .push_str(tag.ok_or_else(|| anyhow!("{{tag}} requires an explicit release tag"))?),
            _ => bail!(
                "unsupported placeholder {placeholder}; only {{version}} and {{tag}} are allowed"
            ),
        }
        remaining = &remaining[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn parse_public_url(value: &str, field: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("{field} is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("{field} must use HTTP or HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{field} must not contain credentials because it will be published");
    }
    if url.host_str().is_none() {
        bail!("{field} must have a host");
    }
    Ok(url)
}

fn validate_filename(filename: &str) -> Result<()> {
    validate_clean_value("filename", filename)?;
    if filename == "." || filename == ".." {
        bail!("filename must name a file");
    }
    if filename.contains(['/', '\\']) {
        bail!("filename must not contain path separators");
    }
    if filename.chars().any(char::is_control) {
        bail!("filename must not contain control characters");
    }
    Ok(())
}

fn validate_optional_clean_value(field: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        validate_clean_value(field, value)?;
    }
    Ok(())
}

fn validate_clean_value(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    if value != value.trim() {
        bail!("{field} must not have leading or trailing whitespace");
    }
    if value.chars().any(char::is_control) {
        bail!("{field} must not contain control characters");
    }
    Ok(())
}

fn normalize_string_list(field: &str, values: &mut Vec<String>) -> Result<()> {
    for value in values.iter_mut() {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("{field} entries must not be empty");
        }
        if trimmed.chars().any(char::is_control) {
            bail!("{field} entries must not contain control characters");
        }
        if trimmed.len() != value.len() {
            *value = trimmed.to_owned();
        }
    }
    deduplicate(values);
    Ok(())
}

fn deduplicate(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use super::*;

    const COMPLETE_MANIFEST: &str = r#"
schema: 1
application: ngit
channel: main
notes: Release notes
commit: main
publication:
  blossom_servers:
    - " https://blossom.example.com "
    - https://blossom.example.com
    - https://mirror.example.com
  relays:
    - " wss://relay.example.com "
    - wss://relay.example.com
  zapstore_relay: true
  strict_metadata: true
  allow_partial_platforms: true
  add_application_platforms: true
assets:
  - source: https://downloads.example.com/ngit/{version}/ngit-{tag}.tar.gz
    identifier: dev.ngit.cli
    version: 2.7.0
    filename: ngit-{tag}.tar.gz
    mime: application/gzip
    platforms: [linux-x86_64, linux-x86_64]
    min_platform_version: "5.10"
    target_platform_version: "6.8"
    supported_nips: ["34", "34", "65"]
    variant: glibc
    commit: 0123456789abcdef
    min_allowed_version: 2.6.0
    android:
      version_code: 27
      min_allowed_version_code: 26
      certificate_sha256:
        - AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    original_url: https://example.com/releases/{tag}
"#;

    #[test]
    fn parses_normalizes_and_resolves_complete_manifest() {
        let manifest = parse_release_manifest(COMPLETE_MANIFEST).unwrap();
        assert_eq!(manifest.commit.as_deref(), Some("main"));
        assert_eq!(
            manifest.publication.blossom_servers,
            ["https://blossom.example.com", "https://mirror.example.com"]
        );
        assert_eq!(manifest.publication.relays, ["wss://relay.example.com"]);
        assert!(manifest.publication.zapstore_relay);
        assert!(manifest.publication.strict_metadata);
        assert!(manifest.publication.allow_partial_platforms);
        assert!(manifest.publication.add_application_platforms);
        assert_eq!(manifest.assets[0].platforms, ["linux-x86_64"]);
        assert_eq!(manifest.assets[0].supported_nips, ["34", "65"]);
        assert_eq!(
            manifest.assets[0]
                .android
                .as_ref()
                .unwrap()
                .certificate_sha256,
            ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        );

        let resolved = manifest.resolve("2.7.0/rc 1", Some("v2.7.0-rc.1")).unwrap();
        assert_eq!(resolved.commit.as_deref(), Some("main"));
        assert_eq!(resolved.publication, manifest.publication);
        let asset = &resolved.assets[0];
        assert_eq!(
            asset.source,
            ResolvedReleaseManifestSource::Url(
                "https://downloads.example.com/ngit/2.7.0%2Frc%201/ngit-v2.7.0-rc.1.tar.gz"
                    .to_owned()
            )
        );
        assert_eq!(asset.filename.as_deref(), Some("ngit-v2.7.0-rc.1.tar.gz"));
        assert_eq!(
            asset.original_url.as_deref(),
            Some("https://example.com/releases/v2.7.0-rc.1")
        );
    }

    #[test]
    fn rejects_unknown_schema_and_keys() {
        let schema_error = parse_release_manifest(
            "schema: 2\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
        )
        .unwrap_err();
        assert!(
            schema_error
                .to_string()
                .contains("unsupported release manifest schema")
        );

        for yaml in [
            "schema: 1\nunknown: true\nassets: []\n",
            "schema: 1\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n    platfroms: [linux]\n",
            "schema: 1\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n    android:\n      certificate_sha: []\n",
            "schema: 1\npublication:\n  blossom_server: https://example.com\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
        ] {
            let error = parse_release_manifest(yaml).unwrap_err();
            assert!(format!("{error:#}").contains("unknown field"));
        }
    }

    #[test]
    fn requires_exactly_one_asset_source_kind() {
        let both = parse_release_manifest(
            r#"
schema: 1
assets:
  - source: https://example.com/app.apk
    file: dist/app.apk
    platforms: [android-arm64-v8a]
"#,
        )
        .unwrap_err();
        assert!(format!("{both:#}").contains("source and file are mutually exclusive"));

        let neither = parse_release_manifest("schema: 1\nassets:\n  - platform_agnostic: true\n")
            .unwrap_err();
        assert!(format!("{neither:#}").contains("exactly one of source or file"));
    }

    #[test]
    fn rejects_duplicate_yaml_keys() {
        let error = parse_release_manifest(
            "schema: 1\nschema: 1\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("duplicate field"));
    }

    #[test]
    fn requires_a_platform_decision() {
        let missing =
            parse_release_manifest("schema: 1\nassets:\n  - source: https://example.com/a\n")
                .unwrap_err();
        assert!(format!("{missing:#}").contains("provide at least one platform"));

        let conflict = parse_release_manifest(
            "schema: 1\nassets:\n  - source: https://example.com/a\n    platforms: [linux-x86_64]\n    platform_agnostic: true\n",
        )
        .unwrap_err();
        assert!(format!("{conflict:#}").contains("mutually exclusive"));
    }

    #[test]
    fn rejects_empty_supplied_values() {
        for yaml in [
            "schema: 1\napplication: '  '\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
            "schema: 1\nrelease_notes: '  '\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
            "schema: 1\nassets: []\n",
            "schema: 1\nassets:\n  - source: ''\n    platform_agnostic: true\n",
            "schema: 1\nassets:\n  - source: https://example.com/a\n    platforms: ['  ']\n",
        ] {
            assert!(parse_release_manifest(yaml).is_err(), "accepted:\n{yaml}");
        }
    }

    #[test]
    fn accepts_release_notes_path_and_rejects_inline_notes_conflict() {
        let manifest = parse_release_manifest(
            "schema: 1\nrelease_notes: CHANGELOG.md\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
        )
        .unwrap();
        assert_eq!(manifest.release_notes.as_deref(), Some("CHANGELOG.md"));
        assert_eq!(
            manifest
                .resolve("1.2.3", None)
                .unwrap()
                .release_notes
                .as_deref(),
            Some(Path::new("CHANGELOG.md"))
        );

        let error = parse_release_manifest(
            "schema: 1\nnotes: Inline\nrelease_notes: CHANGELOG.md\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("notes and release_notes are mutually exclusive"));
    }

    #[test]
    fn extracts_matching_keep_a_changelog_section() {
        let changelog = r#"# Changelog

## [Unreleased]

- Not released.

## [1.2.3] - 2026-08-31

### Added

- Manifest release notes.

### Fixed

- A release bug.

## [1.2.2] - 2026-08-01

- Previous release.
"#;
        let expected = "### Added\n\n- Manifest release notes.\n\n### Fixed\n\n- A release bug.";
        assert_eq!(
            extract_keep_a_changelog_release_notes(changelog, "1.2.3").unwrap(),
            expected
        );
        assert_eq!(
            extract_keep_a_changelog_release_notes(changelog, "v1.2.3").unwrap(),
            expected
        );
    }

    #[test]
    fn supports_unbracketed_version_headings() {
        let changelog = "## v2.0.0 - Stable\n\nRelease notes.\n\n## 1.9.0\n\nOld notes.\n";
        assert_eq!(
            extract_keep_a_changelog_release_notes(changelog, "2.0.0").unwrap(),
            "Release notes."
        );
    }

    #[test]
    fn rejects_missing_duplicate_and_empty_changelog_sections() {
        let missing =
            extract_keep_a_changelog_release_notes("## [1.0.0] - 2026-01-01\n\nNotes.\n", "2.0.0")
                .unwrap_err();
        assert!(missing.to_string().contains("does not contain"));

        let duplicate = extract_keep_a_changelog_release_notes(
            "## [2.0.0]\n\nOne.\n\n## [v2.0.0]\n\nTwo.\n",
            "2.0.0",
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("multiple"));

        let empty = extract_keep_a_changelog_release_notes("## [2.0.0]\n\n## [1.0.0]\n", "2.0.0")
            .unwrap_err();
        assert!(empty.to_string().contains("is empty"));
    }

    #[test]
    fn tag_placeholder_requires_explicit_tag() {
        let manifest = parse_release_manifest(COMPLETE_MANIFEST).unwrap();
        let error = manifest.resolve("2.7.0", None).unwrap_err();
        assert!(format!("{error:#}").contains("requires an explicit release tag"));
    }

    #[test]
    fn rejects_unknown_and_malformed_placeholders() {
        for source in [
            "https://example.com/{branch}/a",
            "https://example.com/{version/a",
            "https://example.com/version}/a",
        ] {
            let yaml = format!(
                "schema: 1\nassets:\n  - source: '{source}'\n    platform_agnostic: true\n"
            );
            assert!(parse_release_manifest(&yaml).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn expansion_is_single_pass() {
        let manifest = parse_release_manifest(
            "schema: 1\nassets:\n  - source: https://example.com/{version}/a\n    platform_agnostic: true\n",
        )
        .unwrap();
        let resolved = manifest.resolve("{tag}", None).unwrap();
        assert_eq!(
            resolved.assets[0].source,
            ResolvedReleaseManifestSource::Url("https://example.com/%7Btag%7D/a".to_owned())
        );
    }

    #[test]
    fn rejects_duplicate_resolved_urls_local_paths_and_filenames() {
        let duplicate_url = parse_release_manifest(
            r#"
schema: 1
assets:
  - source: https://example.com/{version}/a
    filename: one
    platform_agnostic: true
  - source: https://example.com/1/a
    filename: two
    platform_agnostic: true
"#,
        )
        .unwrap();
        let error = duplicate_url.resolve("1", None).unwrap_err();
        assert!(error.to_string().contains("same URL"));

        let duplicate_path = parse_release_manifest(
            r#"
schema: 1
assets:
  - file: dist/{version}/ngit.zip
    filename: one.zip
    platforms: [linux-x86_64]
  - file: dist/1/ngit.zip
    filename: two.zip
    platforms: [linux-x86_64]
"#,
        )
        .unwrap();
        let error = duplicate_path.resolve("1", None).unwrap_err();
        assert!(error.to_string().contains("same local path"));

        let duplicate_filename = parse_release_manifest(
            r#"
schema: 1
assets:
  - source: https://example.com/a
    filename: ngit-{version}.tar.gz
    platform_agnostic: true
  - source: https://example.com/b
    filename: ngit-1.tar.gz
    platform_agnostic: true
"#,
        )
        .unwrap();
        let error = duplicate_filename.resolve("1", None).unwrap_err();
        assert!(error.to_string().contains("same filename"));
    }

    #[test]
    fn parses_and_resolves_android_local_file_metadata() {
        let manifest = parse_release_manifest(
            r#"
schema: 1
application: com.example.app
channel: beta
assets:
  - file: artifacts/{tag}/app-{version}.apk
    identifier: com.example.app.android
    version: 42.0-beta
    filename: example-{tag}.apk
    mime: application/vnd.android.package-archive
    platforms: [android-arm64-v8a, android-x86_64]
    min_platform_version: "26"
    target_platform_version: "35"
    variant: play
    min_allowed_version: "41.0"
    android:
      version_code: 4200
      min_allowed_version_code: 4100
      certificate_sha256:
        - AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    original_url: https://example.com/releases/{version}/{tag}
"#,
        )
        .unwrap();

        let resolved = manifest
            .resolve("42.0/beta 1", Some("v42.0-beta.1"))
            .unwrap();
        let asset = &resolved.assets[0];
        assert_eq!(
            asset.source,
            ResolvedReleaseManifestSource::File(PathBuf::from(
                "artifacts/v42.0-beta.1/app-42.0/beta 1.apk"
            ))
        );
        assert_eq!(asset.filename.as_deref(), Some("example-v42.0-beta.1.apk"));
        assert_eq!(
            asset.original_url.as_deref(),
            Some("https://example.com/releases/42.0%2Fbeta%201/v42.0-beta.1")
        );
        assert_eq!(asset.platforms, ["android-arm64-v8a", "android-x86_64"]);
        assert_eq!(asset.android.as_ref().unwrap().version_code, Some(4200));
        assert_eq!(
            asset.android.as_ref().unwrap().certificate_sha256,
            ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        );
    }

    #[test]
    fn rejects_non_publication_urls_and_hostile_filenames() {
        for source in [
            "file:///tmp/asset",
            "https://user:secret@example.com/asset",
            "/local/asset",
        ] {
            let yaml = format!(
                "schema: 1\nassets:\n  - source: '{source}'\n    platform_agnostic: true\n"
            );
            let manifest = parse_release_manifest(&yaml).unwrap();
            assert!(manifest.resolve("1", None).is_err(), "accepted {source}");
        }

        let manifest = parse_release_manifest(
            "schema: 1\nassets:\n  - source: https://example.com/a\n    filename: '../a'\n    platform_agnostic: true\n",
        )
        .unwrap();
        assert!(manifest.resolve("1", None).is_err());
    }

    #[test]
    fn validates_android_metadata() {
        let missing = parse_release_manifest(
            r#"
schema: 1
assets:
  - source: https://example.com/app.apk
    mime: application/vnd.android.package-archive
    platforms: [android-arm64-v8a]
    android:
      version_code: 1
"#,
        )
        .unwrap_err();
        assert!(format!("{missing:#}").contains("certificate_sha256"));

        let invalid_hash = parse_release_manifest(
            r#"
schema: 1
assets:
  - source: https://example.com/app.apk
    platforms: [android-arm64-v8a]
    android:
      version_code: 1
      certificate_sha256: [not-a-hash]
"#,
        )
        .unwrap_err();
        assert!(format!("{invalid_hash:#}").contains("64 hexadecimal"));
    }

    #[test]
    fn local_apks_may_leave_platforms_for_snapshot_inference() {
        let manifest = parse_release_manifest(
            r#"
schema: 1
assets:
  - file: dist/app.apk
    android:
      version_code: 1
      certificate_sha256:
        - aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#,
        )
        .unwrap();
        assert!(manifest.assets[0].platforms.is_empty());

        let error = parse_release_manifest(
            r#"
schema: 1
assets:
  - file: dist/app.apk
    platform_agnostic: true
    android:
      version_code: 1
      certificate_sha256:
        - aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("cannot be platform agnostic"));
    }

    #[test]
    fn local_apks_still_require_explicit_android_identity_metadata() {
        let error =
            parse_release_manifest("schema: 1\nassets:\n  - file: dist/app.apk\n").unwrap_err();
        assert!(format!("{error:#}").contains("android metadata block"));
    }

    #[test]
    fn resolves_relative_and_absolute_manifest_paths() {
        let root = Path::new("/repo");
        assert_eq!(
            resolve_release_manifest_path(root, None).unwrap(),
            Path::new("/repo/.ngit/release.yaml")
        );
        assert_eq!(
            resolve_release_manifest_path(root, Some(Path::new("config/release.yml"))).unwrap(),
            Path::new("/repo/config/release.yml")
        );
        assert_eq!(
            resolve_release_manifest_path(root, Some(Path::new("/tmp/release.yml"))).unwrap(),
            Path::new("/tmp/release.yml")
        );
    }

    #[test]
    fn loads_relative_manifest_from_repository_root() {
        let root = tempdir().unwrap();
        let config = root.path().join("config");
        fs::create_dir(&config).unwrap();
        fs::write(
            config.join("release.yml"),
            "schema: 1\nassets:\n  - source: https://example.com/a\n    platform_agnostic: true\n",
        )
        .unwrap();

        let loaded =
            load_release_manifest(root.path(), Some(Path::new("config/release.yml"))).unwrap();
        assert_eq!(loaded.path, config.join("release.yml"));
        assert_eq!(loaded.manifest.schema, 1);
    }
}
