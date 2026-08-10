//! Bounded platform inference for immutable Android package snapshots.

use std::{
    collections::BTreeSet,
    io::{Read, Seek},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use zip::ZipArchive;

use crate::blossom::FileSnapshot;

pub const APK_MIME_TYPE: &str = "application/vnd.android.package-archive";
const ANDROID_MANIFEST: &str = "AndroidManifest.xml";
const MAX_APK_ENTRIES: usize = 100_000;
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
