//! Release-aware discovery and guarded replacement for ngit's own binaries.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{
    client::{Client, Connect as _, get_event_from_global_cache, save_event_in_global_cache},
    release_download::{UrlAssetRequest, download_url_asset_to_path},
    version_check::{
        AvailableUpdate, current_platform, current_variant, latest_update_tag,
        ngit_release_filters, ngit_repo_state_filter, referenced_asset_ids_for_version,
        resolve_update_for_version, state_contains_version, update_relay_urls,
    },
};

pub const INSTALL_RECEIPT_FILENAME: &str = ".ngit-install-receipt.json";
const INSTALL_RECEIPT_SCHEMA: u32 = 1;
const STANDALONE_METHOD: &str = "standalone";

#[derive(Clone, Debug)]
pub enum UpdateDiscovery {
    Current {
        current: String,
        newer_candidate: Option<String>,
    },
    Pending {
        current: String,
        candidate: String,
        stage: PendingStage,
        newer_candidate: Option<String>,
    },
    Ready {
        update: Box<AvailableUpdate>,
        newer_candidate: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingStage {
    Release,
    Assets,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExternalInstallation {
    pub manager: String,
    pub executable: PathBuf,
    pub guidance: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Installation {
    Standalone(StandaloneInstallation),
    External(ExternalInstallation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandaloneInstallation {
    pub directory: PathBuf,
    pub ngit: PathBuf,
    pub git_remote_nostr: PathBuf,
    pub receipt: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstalledUpdate {
    pub version: String,
    pub ngit: PathBuf,
    pub git_remote_nostr: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstallReceipt {
    schema: u32,
    method: String,
    version: String,
}

pub async fn discover_update(
    git_repo_path: Option<&Path>,
    extra_relays: &[String],
    requested: Option<&str>,
) -> Result<UpdateDiscovery> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let requested = requested
        .map(|version| {
            Version::parse(version.strip_prefix('v').unwrap_or(version))
                .with_context(|| format!("invalid ngit version {version:?}"))
        })
        .transpose()?;
    let mut relays = update_relay_urls();
    relays.extend(extra_relays.iter().cloned());
    relays.sort();
    relays.dedup();

    let client = Client::default();
    let mut state_events = client
        .get_events(relays.clone(), vec![ngit_repo_state_filter()])
        .await?;
    cache_events(git_repo_path, &state_events).await;
    merge_cached_events(
        git_repo_path,
        &mut state_events,
        vec![ngit_repo_state_filter()],
    )
    .await;
    ensure!(
        !state_events.is_empty(),
        "could not retrieve ngit's signed repository state from the update relays or local cache"
    );
    let newer_candidate = requested
        .as_ref()
        .and_then(|version| latest_update_tag(&state_events, &version.to_string()))
        .map(|version| normalized_version(&version));
    let candidate = if let Some(requested) = requested {
        let requested = requested.to_string();
        ensure!(
            state_contains_version(&state_events, &requested),
            "ngit v{requested} is not present in the signed repository state"
        );
        if requested == current {
            client.disconnect().await?;
            return Ok(UpdateDiscovery::Current {
                current,
                newer_candidate,
            });
        }
        requested
    } else if let Some(candidate) = latest_update_tag(&state_events, &current) {
        candidate
    } else {
        client.disconnect().await?;
        return Ok(UpdateDiscovery::Current {
            current,
            newer_candidate: None,
        });
    };

    let release_filters = ngit_release_filters(&candidate);
    let mut release_events = client
        .get_events(relays.clone(), release_filters.clone())
        .await?;
    cache_events(git_repo_path, &release_events).await;
    merge_cached_events(git_repo_path, &mut release_events, release_filters).await;
    let asset_ids = referenced_asset_ids_for_version(&release_events, &candidate);
    if asset_ids.is_empty() {
        client.disconnect().await?;
        return Ok(UpdateDiscovery::Pending {
            current,
            candidate: normalized_version(&candidate),
            stage: PendingStage::Release,
            newer_candidate,
        });
    }

    let asset_filters = vec![nostr::prelude::Filter::new().ids(asset_ids)];
    let mut asset_events = client.get_events(relays, asset_filters.clone()).await?;
    client.disconnect().await?;
    cache_events(git_repo_path, &asset_events).await;
    merge_cached_events(git_repo_path, &mut asset_events, asset_filters).await;
    let update = resolve_update_for_version(
        &current,
        &candidate,
        &release_events,
        &asset_events,
        current_platform(),
        current_variant(),
    );
    Ok(match update {
        None => UpdateDiscovery::Pending {
            current,
            candidate: normalized_version(&candidate),
            stage: PendingStage::Assets,
            newer_candidate,
        },
        Some(update) => UpdateDiscovery::Ready {
            update: Box::new(update),
            newer_candidate,
        },
    })
}

async fn cache_events(git_repo_path: Option<&Path>, events: &[nostr::prelude::Event]) {
    for event in events {
        let _ = save_event_in_global_cache(git_repo_path, event).await;
    }
}

async fn merge_cached_events(
    git_repo_path: Option<&Path>,
    events: &mut Vec<nostr::prelude::Event>,
    filters: Vec<nostr::prelude::Filter>,
) {
    if let Ok(cached) = get_event_from_global_cache(git_repo_path, filters).await {
        events.extend(cached);
    }
    events.sort_by_key(|event| event.id);
    events.dedup_by_key(|event| event.id);
}

fn normalized_version(version: &str) -> String {
    version.strip_prefix('v').unwrap_or(version).to_string()
}

pub fn classify_installation(current_exe: &Path, latest: &str) -> Result<Installation> {
    let executable = current_exe
        .canonicalize()
        .context("failed to resolve the running ngit executable")?;
    if is_nix_store_path(&executable) {
        return Ok(Installation::External(ExternalInstallation {
            manager: "nix".to_string(),
            executable,
            guidance: format!(
                "this ngit is managed by Nix and will not be modified; update the flake, profile, or package that provides ngit v{latest}"
            ),
        }));
    }

    let directory = executable
        .parent()
        .context("the running ngit executable has no parent directory")?
        .to_path_buf();
    let receipt_path = directory.join(INSTALL_RECEIPT_FILENAME);
    if !receipt_path.is_file() {
        let manager = if path_contains(&executable, ".cargo") {
            "cargo"
        } else {
            "unknown"
        };
        let guidance = if manager == "cargo" {
            format!(
                "this ngit appears to be managed by Cargo; run `cargo install ngit --locked --version {latest}`"
            )
        } else {
            format!(
                "this installation has no standalone installer receipt and will not be modified; update it with the tool that installed ngit v{latest}"
            )
        };
        return Ok(Installation::External(ExternalInstallation {
            manager: manager.to_string(),
            executable,
            guidance,
        }));
    }

    let receipt: InstallReceipt = serde_json::from_slice(
        &fs::read(&receipt_path).context("failed to read standalone installer receipt")?,
    )
    .context("standalone installer receipt is invalid JSON")?;
    ensure!(
        receipt.schema == INSTALL_RECEIPT_SCHEMA,
        "unsupported standalone installer receipt schema {}",
        receipt.schema
    );
    ensure!(
        receipt.method == STANDALONE_METHOD,
        "unsupported installer receipt method {:?}",
        receipt.method
    );
    ensure!(
        executable.file_stem() == Some(OsStr::new("ngit")),
        "the installer receipt is beside an executable not named ngit"
    );

    let git_remote_nostr = directory.join(executable_name("git-remote-nostr"));
    ensure!(
        git_remote_nostr.is_file(),
        "standalone installation is missing {}",
        git_remote_nostr.display()
    );
    Ok(Installation::Standalone(StandaloneInstallation {
        directory,
        ngit: executable,
        git_remote_nostr,
        receipt: receipt_path,
    }))
}

fn is_nix_store_path(path: &Path) -> bool {
    path.starts_with("/nix/store")
}

fn path_contains(path: &Path, component: &str) -> bool {
    path.components()
        .any(|part| part.as_os_str() == OsStr::new(component))
}

fn executable_name(name: &'static str) -> &'static str {
    if cfg!(windows) {
        match name {
            "ngit" => "ngit.exe",
            "git-remote-nostr" => "git-remote-nostr.exe",
            _ => name,
        }
    } else {
        name
    }
}

pub async fn install_update(
    installation: &StandaloneInstallation,
    update: &AvailableUpdate,
) -> Result<InstalledUpdate> {
    #[cfg(not(unix))]
    {
        let _ = (installation, update);
        bail!(
            "automatic replacement is not yet supported on this platform; use the generated pinned installer"
        );
    }
    #[cfg(unix)]
    install_update_unix(installation, update).await
}

#[cfg(unix)]
async fn install_update_unix(
    installation: &StandaloneInstallation,
    update: &AvailableUpdate,
) -> Result<InstalledUpdate> {
    use std::os::unix::fs::PermissionsExt;

    let staging = tempfile::Builder::new()
        .prefix(".ngit-update-")
        .tempdir_in(&installation.directory)
        .context("standalone installation directory is not writable")?;
    let archive = staging.path().join("release-asset");
    let mut request = UrlAssetRequest::new(
        update
            .asset
            .url
            .clone()
            .context("release asset has no download URL")?,
    );
    request.filename = update.asset.filename.clone();
    request.mime_type = Some(update.asset.mime.clone());
    let downloaded = download_url_asset_to_path(request, &archive).await?;
    ensure!(
        downloaded.sha256.eq_ignore_ascii_case(&update.asset.sha256),
        "downloaded archive SHA-256 does not match the signed NIP-82 asset"
    );
    if let Some(expected_size) = update.asset.size {
        ensure!(
            downloaded.size == expected_size,
            "downloaded archive size does not match the signed NIP-82 asset"
        );
    }
    File::open(&archive)?.sync_all()?;

    let staged = staging.path().join("bin");
    fs::create_dir(&staged)?;
    extract_binaries(&archive, &downloaded.filename, &staged)?;
    let staged_ngit = staged.join(executable_name("ngit"));
    let staged_remote = staged.join(executable_name("git-remote-nostr"));
    for binary in [&staged_ngit, &staged_remote] {
        ensure!(
            binary.is_file(),
            "release archive is missing {}",
            binary.display()
        );
        fs::set_permissions(binary, fs::Permissions::from_mode(0o755))?;
    }
    verify_binary_version(&staged_ngit, &format!("ngit {}", update.version))?;
    verify_binary_version(&staged_remote, &format!("v{}", update.version))?;

    replace_binaries(
        [
            (&staged_ngit, &installation.ngit),
            (&staged_remote, &installation.git_remote_nostr),
        ],
        &update.version,
        &installation.receipt,
    )?;
    Ok(InstalledUpdate {
        version: update.version.clone(),
        ngit: installation.ngit.clone(),
        git_remote_nostr: installation.git_remote_nostr.clone(),
    })
}

fn extract_binaries(archive: &Path, filename: &str, destination: &Path) -> Result<()> {
    if filename.ends_with(".zip") {
        extract_zip_binaries(archive, destination)
    } else if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        extract_tar_gz_binaries(archive, destination)
    } else {
        bail!("unsupported ngit release archive format for {filename:?}")
    }
}

fn extract_zip_binaries(archive: &Path, destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(archive)?)
        .context("release asset is not a readable ZIP archive")?;
    let mut found = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.is_file() || entry.enclosed_name().is_none() {
            continue;
        }
        let enclosed = entry
            .enclosed_name()
            .context("ZIP entry path is not enclosed")?;
        let Some(name) = enclosed.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if ![executable_name("ngit"), executable_name("git-remote-nostr")].contains(&name) {
            continue;
        }
        ensure!(
            found.insert(name.to_string()),
            "release archive contains duplicate {name}"
        );
        let mut output = File::create(destination.join(name))?;
        io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
    }
    ensure!(
        found.len() == 2,
        "release archive does not contain both ngit binaries"
    );
    Ok(())
}

fn extract_tar_gz_binaries(archive: &Path, destination: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(File::open(archive)?);
    let mut archive = tar::Archive::new(decoder);
    let mut found = BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?;
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if ![executable_name("ngit"), executable_name("git-remote-nostr")].contains(&name) {
            continue;
        }
        ensure!(
            found.insert(name.to_string()),
            "release archive contains duplicate {name}"
        );
        let mut output = File::create(destination.join(name))?;
        io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
    }
    ensure!(
        found.len() == 2,
        "release archive does not contain both ngit binaries"
    );
    Ok(())
}

fn verify_binary_version(binary: &Path, expected: &str) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to execute staged binary {}", binary.display()))?;
    ensure!(
        output.status.success(),
        "staged binary {} failed its version check",
        binary.display()
    );
    let actual = String::from_utf8(output.stdout).context("staged binary version is not UTF-8")?;
    ensure!(
        actual.trim() == expected,
        "staged binary {} reported {:?}, expected {:?}",
        binary.display(),
        actual.trim(),
        expected
    );
    Ok(())
}

#[cfg(unix)]
fn replace_binaries(
    replacements: [(&Path, &Path); 2],
    version: &str,
    receipt_path: &Path,
) -> Result<()> {
    let suffix = format!("ngit-backup-{}", std::process::id());
    let backups = replacements
        .iter()
        .map(|(_, target)| target.with_extension(&suffix))
        .collect::<Vec<_>>();
    for backup in &backups {
        ensure!(
            !backup.exists(),
            "stale update backup exists at {}",
            backup.display()
        );
    }

    let result = (|| -> Result<()> {
        for ((_, target), backup) in replacements.iter().zip(&backups) {
            fs::rename(target, backup)
                .with_context(|| format!("failed to stage backup for {}", target.display()))?;
        }
        for (staged, target) in replacements {
            fs::rename(staged, target)
                .with_context(|| format!("failed to install {}", target.display()))?;
        }
        write_receipt(receipt_path, version)?;
        Ok(())
    })();

    if let Err(error) = result {
        for ((_, target), backup) in replacements.iter().zip(&backups) {
            if backup.exists() {
                let _ = fs::remove_file(target);
                let _ = fs::rename(backup, target);
            }
        }
        return Err(error.context("the previous standalone installation was restored"));
    }
    for backup in backups {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn write_receipt(path: &Path, version: &str) -> Result<()> {
    let receipt = InstallReceipt {
        schema: INSTALL_RECEIPT_SCHEMA,
        method: STANDALONE_METHOD.to_string(),
        version: version.to_string(),
    };
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    ensure!(!temporary.exists(), "stale receipt temporary file exists");
    fs::write(&temporary, serde_json::to_vec_pretty(&receipt)?)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix_installations_are_never_mutated() {
        assert!(is_nix_store_path(Path::new(
            "/nix/store/hash-ngit/bin/ngit"
        )));
        assert!(!is_nix_store_path(Path::new("/opt/ngit/bin/ngit")));
    }

    #[test]
    fn receipt_is_required_before_a_standalone_installation_is_owned() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("ngit");
        fs::write(&executable, b"binary").unwrap();
        let classification = classify_installation(&executable, "3.0.0").unwrap();
        let Installation::External(external) = classification else {
            panic!("unreceipted executable was treated as standalone");
        };
        assert_eq!(external.manager, "unknown");
    }

    #[test]
    fn valid_receipt_owns_only_sibling_binaries() {
        let temp = tempfile::tempdir().unwrap();
        let ngit = temp.path().join("ngit");
        let remote = temp.path().join("git-remote-nostr");
        fs::write(&ngit, b"binary").unwrap();
        fs::write(&remote, b"binary").unwrap();
        write_receipt(&temp.path().join(INSTALL_RECEIPT_FILENAME), "2.6.3").unwrap();

        let Installation::Standalone(installation) = classify_installation(&ngit, "3.0.0").unwrap()
        else {
            panic!("valid receipt was not recognized");
        };
        assert_eq!(installation.ngit, ngit.canonicalize().unwrap());
        assert_eq!(installation.git_remote_nostr, remote);
    }

    #[cfg(unix)]
    #[test]
    fn extracts_only_the_two_binaries_from_a_tarball() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("ngit.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        for (path, body) in [
            ("release/bin/ngit", b"new ngit".as_slice()),
            ("release/bin/git-remote-nostr", b"new helper".as_slice()),
            ("release/README.md", b"ignored".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, path, body).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
        let destination = temp.path().join("bin");
        fs::create_dir(&destination).unwrap();

        extract_tar_gz_binaries(&archive_path, &destination).unwrap();

        assert_eq!(fs::read(destination.join("ngit")).unwrap(), b"new ngit");
        assert_eq!(
            fs::read(destination.join("git-remote-nostr")).unwrap(),
            b"new helper"
        );
        assert!(!destination.join("README.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn binary_replacement_rolls_back_both_targets_on_partial_failure() {
        let temp = tempfile::tempdir().unwrap();
        let staged_ngit = temp.path().join("staged-ngit");
        let missing_staged_remote = temp.path().join("missing-helper");
        let ngit = temp.path().join("ngit");
        let remote = temp.path().join("git-remote-nostr");
        let receipt = temp.path().join(INSTALL_RECEIPT_FILENAME);
        fs::write(&staged_ngit, b"new ngit").unwrap();
        fs::write(&ngit, b"old ngit").unwrap();
        fs::write(&remote, b"old helper").unwrap();
        write_receipt(&receipt, "2.6.3").unwrap();

        let result = replace_binaries(
            [(&staged_ngit, &ngit), (&missing_staged_remote, &remote)],
            "3.0.0",
            &receipt,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&ngit).unwrap(), b"old ngit");
        assert_eq!(fs::read(&remote).unwrap(), b"old helper");
        let receipt: InstallReceipt = serde_json::from_slice(&fs::read(receipt).unwrap()).unwrap();
        assert_eq!(receipt.version, "2.6.3");
    }
}
