//! Bounded platform inference for immutable Android package snapshots.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek},
    path::Path,
};

use anyhow::{Context, Result, bail};
use apk_info::{Apk, CertificateInfo, Signature};
use serde::Serialize;
use zip::ZipArchive;

use crate::blossom::FileSnapshot;

pub const APK_MIME_TYPE: &str = "application/vnd.android.package-archive";
const ANDROID_MANIFEST: &str = "AndroidManifest.xml";
const MAX_APK_ENTRIES: usize = 100_000;
const MAX_APK_ANALYSIS_BYTES: u64 = 1024 * 1024 * 1024;
const UNIVERSAL_ANDROID_PLATFORMS: [&str; 4] = [
    "android-arm64-v8a",
    "android-armeabi-v7a",
    "android-x86",
    "android-x86_64",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApkPlatformInference {
    pub derived_platforms: Vec<String>,
    pub native_libraries_present: bool,
    pub unknown_abis: Vec<String>,
}

/// Objective Android metadata read from the same immutable snapshot uploaded
/// to Blossom.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApkInspection {
    #[serde(flatten)]
    pub platforms: ApkPlatformInference,
    pub package: String,
    pub version_name: String,
    pub version_code: u64,
    pub min_sdk_version: String,
    pub target_sdk_version: String,
    pub certificate_sha256: Vec<String>,
}

pub fn inspect_apk(snapshot: &FileSnapshot) -> Result<ApkInspection> {
    inspect_apk_file_with_size(snapshot.path(), snapshot.size)
}

/// Inspect an APK directly. Release publication uses [`inspect_apk`] so the
/// parsed file is the same immutable snapshot that is hashed and uploaded.
pub fn inspect_apk_file(path: &Path) -> Result<ApkInspection> {
    let size = path
        .metadata()
        .with_context(|| format!("failed to inspect APK size at {}", path.display()))?
        .len();
    inspect_apk_file_with_size(path, size)
}

fn inspect_apk_file_with_size(path: &Path, size: u64) -> Result<ApkInspection> {
    if size > MAX_APK_ANALYSIS_BYTES {
        bail!(
            "APK is {} bytes, exceeding the safe analysis limit of {MAX_APK_ANALYSIS_BYTES}",
            size
        );
    }
    let file =
        File::open(path).with_context(|| format!("failed to open APK at {}", path.display()))?;
    let platforms = inspect_apk_archive(file)?;
    let apk = Apk::new(path).context("failed to parse compiled Android metadata")?;
    let package = required_apk_value("package", apk.get_package_name())?;
    let version_name = required_apk_value("versionName", apk.get_version_name())?;
    let version_code = required_apk_value("versionCode", apk.get_version_code())?
        .parse::<u64>()
        .context("APK versionCode is not an unsigned integer")?;
    if version_code == 0 {
        bail!("APK versionCode must be greater than zero");
    }
    let min_sdk_version = apk
        .get_min_sdk_version()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "1".to_owned());
    let target_sdk_version = apk
        .get_attribute_value("uses-sdk", "targetSdkVersion")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| min_sdk_version.clone());
    let certificate_sha256 = certificate_hashes(apk.get_signatures()?)?;

    Ok(ApkInspection {
        platforms,
        package,
        version_name,
        version_code,
        min_sdk_version,
        target_sdk_version,
        certificate_sha256,
    })
}

fn required_apk_value(field: &str, value: Option<String>) -> Result<String> {
    let value = value.ok_or_else(|| anyhow::anyhow!("APK does not declare {field}"))?;
    if value.trim().is_empty() {
        bail!("APK declares an empty {field}");
    }
    Ok(value)
}

fn certificate_hashes(signatures: Vec<Signature>) -> Result<Vec<String>> {
    let mut hashes = BTreeSet::new();
    for signature in signatures {
        let certificates = match signature {
            Signature::V1(certificates)
            | Signature::V2(certificates)
            | Signature::V3(certificates)
            | Signature::V31(certificates) => certificates,
            _ => continue,
        };
        hashes.extend(certificates.into_iter().map(
            |CertificateInfo {
                 sha256_fingerprint, ..
             }| sha256_fingerprint.to_ascii_lowercase(),
        ));
    }
    if hashes.is_empty() {
        bail!("APK does not contain a recognized v1, v2, v3, or v3.1 signing certificate");
    }
    Ok(hashes.into_iter().collect())
}

pub fn inspect_apk_platforms(snapshot: &FileSnapshot) -> Result<ApkPlatformInference> {
    let file = snapshot.reopen()?;
    inspect_apk_archive(file)
}

fn inspect_apk_archive<R: Read + Seek>(reader: R) -> Result<ApkPlatformInference> {
    let mut archive = ZipArchive::new(reader).context("APK is not a readable ZIP archive")?;
    if archive.len() > MAX_APK_ENTRIES {
        bail!(
            "APK contains {} entries, exceeding the safe limit of {MAX_APK_ENTRIES}",
            archive.len()
        );
    }

    let mut manifest_count = 0_usize;
    let mut platforms = BTreeSet::new();
    let mut unknown_abis = BTreeSet::new();
    let mut native_libraries_present = false;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .with_context(|| format!("failed to inspect APK ZIP entry {index}"))?;
        let name = entry.name();
        if name == ANDROID_MANIFEST {
            manifest_count += 1;
            if entry.is_dir() || entry.size() == 0 {
                bail!("APK root AndroidManifest.xml must be a non-empty file");
            }
        }

        let components = name.split('/').collect::<Vec<_>>();
        if components.len() != 3
            || components[0] != "lib"
            || !components[2].ends_with(".so")
            || components[2].is_empty()
        {
            continue;
        }
        native_libraries_present = true;
        let abi = components[1];
        let platform = match abi {
            "arm64-v8a" => "android-arm64-v8a".to_owned(),
            "armeabi-v7a" => "android-armeabi-v7a".to_owned(),
            "x86" => "android-x86".to_owned(),
            "x86_64" => "android-x86_64".to_owned(),
            _ => {
                validate_unknown_abi(abi)?;
                unknown_abis.insert(abi.to_owned());
                format!("android-{abi}")
            }
        };
        platforms.insert(platform);
    }

    if manifest_count != 1 {
        bail!("APK must contain exactly one root AndroidManifest.xml; found {manifest_count}");
    }
    if !native_libraries_present {
        platforms.extend(UNIVERSAL_ANDROID_PLATFORMS.map(str::to_owned));
    }
    Ok(ApkPlatformInference {
        derived_platforms: platforms.into_iter().collect(),
        native_libraries_present,
        unknown_abis: unknown_abis.into_iter().collect(),
    })
}

fn validate_unknown_abi(abi: &str) -> Result<()> {
    if abi.is_empty()
        || abi.len() > 64
        || !abi
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("APK contains an unsafe native ABI name {abi:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use anyhow::Result;
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::inspect_apk_archive;

    fn apk(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            writer.start_file(*name, options)?;
            writer.write_all(bytes)?;
        }
        Ok(writer.finish()?.into_inner())
    }

    #[test]
    fn native_abis_map_to_nip_82_platforms() -> Result<()> {
        let bytes = apk(&[
            ("AndroidManifest.xml", b"manifest"),
            ("lib/arm64-v8a/libapp.so", b"native"),
            ("lib/riscv64/libapp.so", b"native"),
        ])?;
        let inferred = inspect_apk_archive(Cursor::new(bytes))?;
        assert!(inferred.native_libraries_present);
        assert_eq!(
            inferred.derived_platforms,
            ["android-arm64-v8a", "android-riscv64"]
        );
        assert_eq!(inferred.unknown_abis, ["riscv64"]);
        Ok(())
    }

    #[test]
    fn packages_without_native_libraries_cover_standard_android_abis() -> Result<()> {
        let bytes = apk(&[("AndroidManifest.xml", b"manifest")])?;
        let inferred = inspect_apk_archive(Cursor::new(bytes))?;
        assert!(!inferred.native_libraries_present);
        assert_eq!(
            inferred.derived_platforms,
            [
                "android-arm64-v8a",
                "android-armeabi-v7a",
                "android-x86",
                "android-x86_64",
            ]
        );
        Ok(())
    }

    #[test]
    fn packages_require_one_nonempty_root_manifest() -> Result<()> {
        let missing = apk(&[("lib/x86/libapp.so", b"native")])?;
        assert!(
            inspect_apk_archive(Cursor::new(missing))
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );

        let empty = apk(&[("AndroidManifest.xml", b"")])?;
        assert!(
            inspect_apk_archive(Cursor::new(empty))
                .unwrap_err()
                .to_string()
                .contains("non-empty")
        );
        Ok(())
    }
}
