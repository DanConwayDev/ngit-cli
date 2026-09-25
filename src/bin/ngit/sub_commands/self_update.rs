use std::env;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::{cli::UpdateArgs, output};

pub async fn launch(args: &UpdateArgs) -> Result<()> {
    let current_exe = env::current_exe()?;
    let lookup = inspect_installation(&current_exe).await;
    let git_repo = ngit::git::Repo::discover().ok();
    let git_repo_path = git_repo
        .as_ref()
        .and_then(|repo| ngit::git::RepoActions::get_path(repo).ok());
    match ngit::self_update::discover_update(git_repo_path, &args.relays, args.target.as_deref())
        .await?
    {
        ngit::self_update::UpdateDiscovery::Current {
            current,
            newer_candidate,
        } => {
            finish_current(
                args,
                &lookup,
                &current_exe,
                &current,
                newer_candidate.as_deref(),
            )
            .await?;
        }
        ngit::self_update::UpdateDiscovery::Pending {
            current,
            candidate,
            stage,
            newer_candidate,
        } => {
            set_pending_output(&current, &candidate, stage, newer_candidate.as_deref());
            println!(
                "ngit v{candidate} has been tagged, but its trusted NIP-82 {} not ready yet",
                match stage {
                    ngit::self_update::PendingStage::Release => "release is",
                    ngit::self_update::PendingStage::Assets => "release assets are",
                }
            );
            print_newer_candidate_notice(Some(&candidate), newer_candidate.as_deref());
        }
        ngit::self_update::UpdateDiscovery::Ready {
            update,
            newer_candidate,
        } if args.check => {
            set_ready_output(&update, "available", None, newer_candidate.as_deref());
            println!(
                "ngit v{} is ready for {}; you have v{}",
                update.version,
                ngit::version_check::current_platform().unwrap_or("this platform"),
                update.current
            );
            print_newer_candidate_notice(Some(&update.version), newer_candidate.as_deref());
        }
        ngit::self_update::UpdateDiscovery::Ready {
            update,
            newer_candidate,
        } => {
            match ngit::self_update::classify_installation(&current_exe, &update.version)? {
                ngit::self_update::Installation::External(external)
                    if external.manager == "cargo" =>
                {
                    require_consistent_installation(&lookup)?;
                    update_with_cargo(&update, &external, newer_candidate.as_deref()).await?;
                    report_installation(&current_exe, &update.version).await;
                }
                ngit::self_update::Installation::External(external) => {
                    set_ready_output(
                        &update,
                        "managed_externally",
                        Some(&external),
                        newer_candidate.as_deref(),
                    );
                    println!("{}", external.guidance);
                }
                ngit::self_update::Installation::Standalone(installation) => {
                    update_standalone(
                        &update,
                        &installation,
                        &current_exe,
                        &lookup,
                        newer_candidate.as_deref(),
                    )
                    .await?;
                }
            }
            print_newer_candidate_notice(Some(&update.version), newer_candidate.as_deref());
        }
    }
    Ok(())
}

async fn update_with_cargo(
    update: &ngit::version_check::AvailableUpdate,
    installation: &ngit::self_update::ExternalInstallation,
    newer_candidate: Option<&str>,
) -> Result<()> {
    let installed = ngit::self_update::install_cargo_update(installation, &update.version).await?;
    output::set_value(json!({
        "command_status": "ok",
        "command": "update",
        "result": {
            "state": "installed",
            "method": "cargo",
            "previous_version": update.current,
            "version": installed.version,
            "newer_candidate_version": newer_candidate,
            "ngit": installed.ngit,
            "git_remote_nostr": installed.git_remote_nostr,
            "release_event_id": update.release.raw_event.id.to_hex(),
        }
    }));
    println!(
        "updated ngit from v{} to v{} through Cargo (default registry and default features)",
        update.current, update.version
    );
    Ok(())
}

fn set_current_output(current: &str, newer_candidate: Option<&str>) {
    output::set_value(json!({
        "command_status": "ok",
        "command": "update",
        "result": {
            "state": "current",
            "current_version": current,
            "newer_candidate_version": newer_candidate,
        }
    }));
}

fn set_pending_output(
    current: &str,
    candidate: &str,
    stage: ngit::self_update::PendingStage,
    newer_candidate: Option<&str>,
) {
    output::set_value(json!({
        "command_status": "ok",
        "command": "update",
        "result": {
            "state": "pending",
            "current_version": current,
            "candidate_version": candidate,
            "waiting_for": stage,
            "newer_candidate_version": newer_candidate,
        }
    }));
}

fn set_ready_output(
    update: &ngit::version_check::AvailableUpdate,
    state: &str,
    external: Option<&ngit::self_update::ExternalInstallation>,
    newer_candidate: Option<&str>,
) {
    output::set_value(json!({
        "command_status": "ok",
        "command": "update",
        "result": {
            "state": state,
            "current_version": update.current,
            "version": update.version,
            "newer_candidate_version": newer_candidate,
            "platform": ngit::version_check::current_platform(),
            "variant": ngit::version_check::current_variant(),
            "release_event_id": update.release.raw_event.id.to_hex(),
            "asset": {
                "event_id": update.asset.raw_event.id.to_hex(),
                "url": update.asset.url,
                "filename": update.asset.filename,
                "sha256": update.asset.sha256,
                "size": update.asset.size.map(|size| size.to_string()),
            },
            "installation": external,
        }
    }));
}

fn print_newer_candidate_notice(selected: Option<&str>, newer: Option<&str>) {
    if let (Some(selected), Some(newer)) = (selected, newer) {
        eprintln!("note: ngit v{newer} is newer than the selected v{selected}");
    }
}

async fn report_installation(executable: &std::path::Path, version: &str) {
    // On Unix current_exe may point at the old inode after replacement; use
    // the original install location captured before the update instead.
    match ngit::install_lookup::inspect(executable).await {
        Ok(mut lookup) => {
            lookup.check_version(version).await;
            for warning in lookup.warnings {
                eprintln!("warning: {warning}");
            }
        }
        Err(error) => eprintln!("warning: cannot verify the active installation: {error}"),
    }
}

async fn inspect_installation(
    executable: &std::path::Path,
) -> Result<ngit::install_lookup::Lookup> {
    let mut lookup = ngit::install_lookup::inspect(executable).await;
    if let Ok(lookup) = &mut lookup {
        lookup.check_version(env!("CARGO_PKG_VERSION")).await;
    }
    match &lookup {
        Ok(lookup) => {
            for warning in &lookup.warnings {
                eprintln!("warning: {warning}");
            }
        }
        Err(error) => eprintln!("warning: cannot verify the active installation: {error}"),
    }
    lookup
}

fn require_consistent_installation(lookup: &Result<ngit::install_lookup::Lookup>) -> Result<()> {
    if lookup.as_ref().is_ok_and(|lookup| lookup.conflict) {
        bail!(
            "Resolve the installation lookup warnings before updating; no files were changed. Use the owning package manager, or explicitly migrate with the one-line installer's --method standalone option."
        );
    }
    Ok(())
}

async fn finish_current(
    args: &UpdateArgs,
    lookup: &Result<ngit::install_lookup::Lookup>,
    executable: &std::path::Path,
    current: &str,
    newer: Option<&str>,
) -> Result<()> {
    if !args.check && lookup.as_ref().is_ok_and(|lookup| lookup.conflict) {
        // An unsupported layout is advice, not a failure of an already-current
        // executable. Once cleanup starts, errors must still be reported.
        if current_consolidation_candidate(executable, current).await {
            ngit::install_lookup::consolidate(executable, current).await?;
        }
    }
    set_current_output(current, newer);
    println!("ngit executable v{current} is current");
    print_newer_candidate_notice(Some(current), newer);
    Ok(())
}

async fn current_consolidation_candidate(executable: &std::path::Path, current: &str) -> bool {
    match ngit::install_lookup::consolidation_candidate(executable, current).await {
        Ok(candidate) => candidate.is_some(),
        Err(error) => {
            eprintln!(
                "warning: cannot consolidate automatically: {error:#}. Resolve the conflicting copy through its installation method."
            );
            false
        }
    }
}

async fn update_standalone(
    update: &ngit::version_check::AvailableUpdate,
    installation: &ngit::self_update::StandaloneInstallation,
    current_exe: &std::path::Path,
    lookup: &Result<ngit::install_lookup::Lookup>,
    newer_candidate: Option<&str>,
) -> Result<()> {
    let needs_consolidation = if require_consistent_installation(lookup).is_err() {
        ngit::install_lookup::consolidation_candidate(current_exe, &update.version)
            .await?
            .is_some()
    } else {
        false
    };
    let installed = ngit::self_update::install_update(installation, update).await?;
    let document = json!({
        "command_status": "ok",
        "command": "update",
        "result": {
            "state": "installed",
            "previous_version": update.current,
            "version": installed.version,
            "newer_candidate_version": newer_candidate,
            "ngit": installed.ngit,
            "git_remote_nostr": installed.git_remote_nostr,
            "release_event_id": update.release.raw_event.id.to_hex(),
            "asset_event_id": update.asset.raw_event.id.to_hex(),
        }
    });
    let consolidation = if needs_consolidation {
        ngit::install_lookup::consolidate(current_exe, &update.version)
            .await
            .context("ngit was updated, but standalone consolidation could not finish")
    } else {
        Ok(())
    };
    output::set_value(with_consolidation_result(document, &consolidation));
    consolidation?;
    report_installation(current_exe, &update.version).await;
    println!(
        "updated ngit from v{} to v{}",
        update.current, update.version
    );
    Ok(())
}

// Preserve the successful replacement details even when cleanup makes the
// overall command fail. finish_error emits an already-stored document as-is.
fn with_consolidation_result(
    mut document: serde_json::Value,
    consolidation: &Result<()>,
) -> serde_json::Value {
    if let Err(error) = consolidation {
        document["command_status"] = json!("error");
        document["error"] = json!(format!("{error:#}"));
        document["result"]["consolidation"] = json!({
            "state": "failed",
            "error": format!("{error:#}"),
        });
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_cleanup_reports_error_without_losing_successful_installation() {
        let installed = json!({
            "command_status": "ok",
            "command": "update",
            "result": { "state": "installed", "version": "3.0.3" }
        });
        assert_eq!(
            with_consolidation_result(installed.clone(), &Ok(())),
            installed
        );
        let failed = with_consolidation_result(installed, &Err(anyhow::anyhow!("cleanup failed")));
        assert_eq!(failed["command_status"], "error");
        assert_eq!(failed["result"]["state"], "installed");
        assert_eq!(failed["result"]["version"], "3.0.3");
        assert_eq!(failed["result"]["consolidation"]["state"], "failed");
        assert!(failed["error"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn unverified_lookup_does_not_block_owned_updates() {
        assert!(require_consistent_installation(&Err(anyhow::anyhow!("Git unavailable"))).is_ok());
    }

    #[tokio::test]
    async fn current_installation_without_a_candidate_remains_current() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            !current_consolidation_candidate(&directory.path().join("missing-ngit"), "3.0.3").await
        );
    }
}
