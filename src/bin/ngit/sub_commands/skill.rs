use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ngit::{
    agent_guidance,
    client::get_repo_ref_from_cache,
    git::{Repo, RepoActions},
    login::{
        SignerInfo,
        existing::{command_signer_public_key, resolve_selection},
        get_likely_logged_in_user,
    },
};
use serde::Serialize;

use crate::cli::{SignerParams, SkillCommands, SkillOptOutArgs};

struct SkillContext {
    repo: Repo,
    root: std::path::PathBuf,
}

#[derive(Serialize)]
struct Output {
    command_status: &'static str,
    #[serde(flatten)]
    guidance: agent_guidance::GuidanceStatus,
    is_maintainer: Option<bool>,
    reminders_enabled: bool,
}

#[derive(Serialize)]
struct ReconcileOutput {
    command_status: &'static str,
    action: &'static str,
    changed_files: Vec<String>,
    changes_uncommitted: bool,
    is_maintainer: Option<bool>,
    #[serde(flatten)]
    guidance: agent_guidance::GuidanceStatus,
}

#[derive(Default)]
struct UncommittedPathReport {
    changed_files: Vec<String>,
    warnings: Vec<String>,
}

pub async fn launch(
    command: &SkillCommands,
    force: bool,
    json: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    if let SkillCommands::OptOut(args) = command {
        return opt_out(args);
    }

    let context = resolve_context()?;
    match command {
        SkillCommands::Install | SkillCommands::Upgrade => {
            reconcile(&context, force, json, auth).await
        }
        SkillCommands::Status => {
            let output = Output {
                command_status: "ok",
                guidance: agent_guidance::status(&context.root)?,
                is_maintainer: resolve_maintainer(&context, auth).await,
                reminders_enabled: agent_guidance::reminders_enabled(&context.repo)?,
            };
            if json {
                crate::output::set(output)?;
            } else {
                print_status(&output);
            }
            Ok(())
        }
        SkillCommands::OptOut(_) => unreachable!("opt-out handled before resolving a worktree"),
    }
}

fn opt_out(args: &SkillOptOutArgs) -> Result<()> {
    if args.global {
        agent_guidance::set_global_reminders_enabled(false)?;
        eprintln!(
            "disabled ngit repository skill reminders globally; re-enable with `git config --global nostr.skill-reminders true`"
        );
    } else {
        let context = resolve_context()?;
        agent_guidance::set_reminders_enabled(&context.repo, false)?;
        eprintln!(
            "disabled ngit repository skill reminders for this repository; re-enable with `git config nostr.skill-reminders true`"
        );
    }
    Ok(())
}

async fn reconcile(
    context: &SkillContext,
    force: bool,
    json: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let is_maintainer = resolve_maintainer(context, auth).await;
    reconcile_for_account(context, force, json, is_maintainer)
}

fn reconcile_for_account(
    context: &SkillContext,
    force: bool,
    json: bool,
    is_maintainer: Option<bool>,
) -> Result<()> {
    let before = agent_guidance::status(&context.root)?;
    let mut attempted_paths = vec![];
    let update = agent_guidance::update_with_report(&context.root, force, &mut |path: &Path| {
        let path = path.to_path_buf();
        if !attempted_paths.contains(&path) {
            attempted_paths.push(path);
        }
    });
    let inspection = uncommitted_paths(context, &attempted_paths);
    for warning in inspection.warnings {
        eprintln!("warning: {warning}");
    }
    let changed_files = inspection.changed_files;
    let changes_uncommitted = !changed_files.is_empty();
    let after = match update {
        Ok(after) => after,
        Err(error) => {
            print_changed_files(&changed_files, true, is_maintainer);
            if json {
                crate::output::set_value(serde_json::json!({
                    "command_status": "error",
                    "action": "failed",
                    "error": format!("{error:#}"),
                    "changed_files": changed_files,
                    "changes_uncommitted": changes_uncommitted,
                    "is_maintainer": is_maintainer,
                }));
            }
            return Err(error);
        }
    };
    let action;
    if !before.installed {
        action = "installed";
        eprintln!(
            "installed ngit repository skill version {}",
            after.bundled_version
        );
    } else if before.update_available {
        action = "upgraded";
        eprintln!(
            "upgraded ngit repository skill from {} to {}",
            before.installed_version.as_deref().unwrap_or("unknown"),
            after.bundled_version
        );
    } else if attempted_paths.is_empty() {
        action = "unchanged";
        eprintln!(
            "ngit repository skill is already at bundled version {}",
            after.bundled_version
        );
    } else {
        action = "reconciled";
        eprintln!(
            "reconciled ngit repository skill at bundled version {}",
            after.bundled_version
        );
    }
    print_changed_files(&changed_files, false, is_maintainer);
    if json {
        crate::output::set(ReconcileOutput {
            command_status: "ok",
            action,
            changed_files,
            changes_uncommitted,
            is_maintainer,
            guidance: after,
        })?;
    }
    Ok(())
}

fn uncommitted_paths(context: &SkillContext, attempted_paths: &[PathBuf]) -> UncommittedPathReport {
    inspect_uncommitted_paths(attempted_paths, |relative| {
        context.repo.git_repo.status_file(relative)
    })
}

fn inspect_uncommitted_paths(
    attempted_paths: &[PathBuf],
    mut status_file: impl FnMut(&Path) -> std::result::Result<git2::Status, git2::Error>,
) -> UncommittedPathReport {
    let mut report = UncommittedPathReport::default();
    for relative in attempted_paths {
        let status = match status_file(relative) {
            Ok(status) => status,
            Err(error) if error.code() == git2::ErrorCode::NotFound => continue,
            Err(error) => {
                report.warnings.push(format!(
                    "failed to inspect repository skill path `{}`: {error}",
                    relative.display()
                ));
                continue;
            }
        };
        if status == git2::Status::CURRENT || status.contains(git2::Status::IGNORED) {
            continue;
        }
        let relative = relative.to_string_lossy().into_owned();
        if !report.changed_files.contains(&relative) {
            report.changed_files.push(relative);
        }
    }
    report
}

fn print_changed_files(changed_files: &[String], partial: bool, is_maintainer: Option<bool>) {
    if changed_files.is_empty() {
        return;
    }
    if partial {
        eprintln!("repository skill update stopped with changed files:");
    } else {
        eprintln!("changed repository skill files:");
    }
    for path in changed_files {
        eprintln!("  {path}");
    }
    eprintln!(
        "changes remain uncommitted; review and commit them with this repository's normal validation workflow"
    );
    if is_maintainer == Some(false) {
        eprintln!(
            "tip: you are not a repository maintainer; commit these changes on a `pr/` branch to open a pull request"
        );
    }
}

fn print_status(output: &Output) {
    println!("installed: {}", output.guidance.installed);
    println!(
        "installed version: {}",
        output
            .guidance
            .installed_version
            .as_deref()
            .unwrap_or("none")
    );
    println!("bundled version: {}", output.guidance.bundled_version);
    println!("update available: {}", output.guidance.update_available);
    println!(
        "locally modified files: {}",
        if output.guidance.modified_files.is_empty() {
            "none".into()
        } else {
            output.guidance.modified_files.join(", ")
        }
    );
    println!(
        "current account is maintainer: {}",
        output
            .is_maintainer
            .map_or("unknown", |value| if value { "true" } else { "false" })
    );
    println!("reminders enabled: {}", output.reminders_enabled);
    println!(
        "managed files: {}",
        output.guidance.managed_files.join(", ")
    );
}

fn resolve_context() -> Result<SkillContext> {
    let repo = Repo::discover().context("ngit skill must run inside a Git working tree")?;
    let root = repo.get_path()?.to_path_buf();
    Ok(SkillContext { repo, root })
}

async fn resolve_maintainer(context: &SkillContext, auth: SignerParams<'_>) -> Option<bool> {
    // Status remains useful without a Nostr remote, cached announcement, or
    // login. Resolve this extra metadata only when every local lookup works.
    let (_, decoded) = context
        .repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await
        .ok()??;
    let repo_ref = get_repo_ref_from_cache(Some(&context.root), &decoded.coordinate)
        .await
        .ok()?;
    let account = resolve_account(context, auth).await.ok()??;
    Some(repo_ref.is_authorized_maintainer(&account))
}

async fn resolve_account(
    context: &SkillContext,
    auth: SignerParams<'_>,
) -> Result<Option<nostr::prelude::PublicKey>> {
    if let Some(signer_info) = auth.info {
        if let SignerInfo::Selection { selector } = signer_info {
            let selected =
                resolve_selection(&Some(&context.repo), selector, auth.password, true).await?;
            return nostr::prelude::PublicKey::parse(&selected.npub)
                .context("selected signer has an invalid npub")
                .map(Some);
        }
        return command_signer_public_key(signer_info, auth.password);
    }
    get_likely_logged_in_user(&context.root).await
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use nostr::prelude::Keys;

    use super::*;

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    fn repository() -> (Repo, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ngit-skill-command-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let git_repo = git2::Repository::init(&root).unwrap();
        let mut config = git_repo.config().unwrap();
        config.set_str("user.name", "ngit test").unwrap();
        config
            .set_str("user.email", "test@example.invalid")
            .unwrap();
        fs::write(root.join("README.md"), "initial\n").unwrap();
        let mut index = git_repo.index().unwrap();
        index.add_path(std::path::Path::new("README.md")).unwrap();
        let tree_id = index.write_tree().unwrap();
        index.write().unwrap();
        let signature = git_repo.signature().unwrap();
        git_repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "initial",
                &git_repo.find_tree(tree_id).unwrap(),
                &[],
            )
            .unwrap();
        (Repo { git_repo }, root)
    }

    #[tokio::test]
    async fn command_signer_overrides_the_configured_account() {
        let (repo, root) = repository();
        let configured = Keys::generate();
        let selected = Keys::generate();
        repo.save_git_config_item("nostr.npub", &configured.public_key().to_string(), false)
            .unwrap();
        repo.save_git_config_item(
            "nostr.signer-alias.selected",
            &selected.public_key().to_string(),
            false,
        )
        .unwrap();
        repo.save_git_config_item("nostr.nsec", &selected.secret_key().to_secret_hex(), false)
            .unwrap();
        let context = SkillContext { repo, root };
        let info = Some(SignerInfo::Selection {
            selector: "selected".to_string(),
        });
        let password = None;

        assert_eq!(
            resolve_account(
                &context,
                SignerParams {
                    info: &info,
                    password: &password,
                },
            )
            .await
            .unwrap(),
            Some(selected.public_key())
        );
        assert_eq!(
            resolve_account(
                &context,
                SignerParams {
                    info: &None,
                    password: &password,
                },
            )
            .await
            .unwrap(),
            Some(configured.public_key())
        );
    }

    #[tokio::test]
    async fn command_nsec_overrides_the_configured_account() {
        let (repo, root) = repository();
        let configured = Keys::generate();
        let selected = Keys::generate();
        repo.save_git_config_item("nostr.npub", &configured.public_key().to_string(), false)
            .unwrap();
        let context = SkillContext { repo, root };
        let info = Some(SignerInfo::Nsec {
            nsec: selected.secret_key().to_secret_hex(),
            password: None,
            npub: None,
            verify_npub: false,
        });
        let password = None;

        assert_eq!(
            resolve_account(
                &context,
                SignerParams {
                    info: &info,
                    password: &password,
                },
            )
            .await
            .unwrap(),
            Some(selected.public_key())
        );
    }

    #[tokio::test]
    async fn unresolved_command_bunker_does_not_use_the_configured_account() {
        let (repo, root) = repository();
        let configured = Keys::generate();
        repo.save_git_config_item("nostr.npub", &configured.public_key().to_string(), false)
            .unwrap();
        let context = SkillContext { repo, root };
        let info = Some(SignerInfo::Bunker {
            bunker_uri: "unused".to_string(),
            bunker_app_key: "unused".to_string(),
            npub: None,
        });
        let password = None;

        assert_eq!(
            resolve_account(
                &context,
                SignerParams {
                    info: &info,
                    password: &password,
                },
            )
            .await
            .unwrap(),
            None
        );
    }

    #[test]
    fn install_leaves_changes_uncommitted() {
        let (repo, root) = repository();
        let context = SkillContext {
            repo,
            root: root.clone(),
        };
        let original_head = context
            .repo
            .git_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap();

        reconcile_for_account(&context, false, false, None).unwrap();

        let head = context
            .repo
            .git_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let mut index = context.repo.git_repo.index().unwrap();
        assert_eq!(head.id(), original_head.id());
        assert_eq!(index.write_tree().unwrap(), original_head.tree_id());
        assert!(
            context
                .repo
                .git_repo
                .status_file(std::path::Path::new(agent_guidance::SKILL_PATH))
                .unwrap()
                .contains(git2::Status::WT_NEW)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_preserves_existing_index_changes() {
        let (repo, root) = repository();
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        let mut index = repo.git_repo.index().unwrap();
        index.add_path(std::path::Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        let context = SkillContext {
            repo,
            root: root.clone(),
        };

        reconcile_for_account(&context, false, false, None).unwrap();

        assert!(root.join(agent_guidance::SKILL_PATH).is_file());
        assert!(
            context
                .repo
                .git_repo
                .status_file(std::path::Path::new("staged.txt"))
                .unwrap()
                .contains(git2::Status::INDEX_NEW)
        );
        assert!(
            context
                .repo
                .git_repo
                .status_file(std::path::Path::new(agent_guidance::SKILL_PATH))
                .unwrap()
                .contains(git2::Status::WT_NEW)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_inspection_errors_preserve_other_observed_changes() {
        let paths = vec![PathBuf::from("unreadable"), PathBuf::from("changed")];
        let report = inspect_uncommitted_paths(&paths, |path| {
            if path == Path::new("unreadable") {
                Err(git2::Error::from_str("corrupt index"))
            } else {
                Ok(git2::Status::WT_MODIFIED)
            }
        });

        assert_eq!(report.changed_files, vec!["changed".to_string()]);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("unreadable"));
        assert!(report.warnings[0].contains("corrupt index"));
    }
}
