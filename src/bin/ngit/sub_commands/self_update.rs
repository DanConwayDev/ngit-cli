use std::env;

use anyhow::Result;
use serde_json::json;

use crate::{cli::UpdateArgs, output};

pub async fn launch(args: &UpdateArgs) -> Result<()> {
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
            set_current_output(&current, newer_candidate.as_deref());
            println!("ngit v{current} is current");
            print_newer_candidate_notice(Some(&current), newer_candidate.as_deref());
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
            let current_exe = env::current_exe()?;
            match ngit::self_update::classify_installation(&current_exe, &update.version)? {
                ngit::self_update::Installation::External(external)
                    if external.manager == "cargo" =>
                {
                    update_with_cargo(&update, &external, newer_candidate.as_deref()).await?;
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
                    let installed =
                        ngit::self_update::install_update(&installation, &update).await?;
                    output::set_value(json!({
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
                    }));
                    println!(
                        "updated ngit from v{} to v{}",
                        update.current, update.version
                    );
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
        "updated ngit from v{} to v{} through Cargo",
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
