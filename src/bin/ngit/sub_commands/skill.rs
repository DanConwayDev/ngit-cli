use anyhow::{Context, Result};
use ngit::{
    agent_guidance,
    client::get_repo_ref_from_cache,
    git::{Repo, RepoActions},
    login::get_likely_logged_in_user,
};
use serde::Serialize;

use crate::cli::{SkillCommands, SkillOptOutArgs};

struct SkillContext {
    repo: Repo,
    root: std::path::PathBuf,
}

#[derive(Serialize)]
struct Output {
    #[serde(flatten)]
    guidance: agent_guidance::GuidanceStatus,
    is_maintainer: Option<bool>,
    reminders_enabled: bool,
}

pub async fn launch(command: &SkillCommands, force: bool) -> Result<()> {
    if let SkillCommands::OptOut(args) = command {
        return opt_out(args);
    }

    let context = resolve_context()?;
    match command {
        SkillCommands::Install | SkillCommands::Upgrade => reconcile(&context, force).await,
        SkillCommands::Status { json } => {
            let output = Output {
                guidance: agent_guidance::status(&context.root)?,
                is_maintainer: resolve_maintainer(&context).await,
                reminders_enabled: agent_guidance::reminders_enabled(&context.repo)?,
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(&output)?);
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

async fn reconcile(context: &SkillContext, force: bool) -> Result<()> {
    let is_maintainer = resolve_maintainer(context).await;
    reconcile_for_account(context, force, is_maintainer)
}

fn reconcile_for_account(
    context: &SkillContext,
    force: bool,
    is_maintainer: Option<bool>,
) -> Result<()> {
    let before = agent_guidance::status(&context.root)?;
    let paths_for_commit = agent_guidance::paths_for_commit(&context.root)?;
    let changes_needed = !paths_for_commit.is_empty();
    let commit = changes_needed && is_maintainer == Some(true);
    let commit_preflight = commit.then(|| {
        agent_guidance::preflight_dedicated_commit(&context.repo, &context.root, &paths_for_commit)
    });
    agent_guidance::update(&context.root, force)?;
    let commit_created = match commit_preflight {
        Some(Ok(())) => {
            match agent_guidance::commit_guidance(&context.repo, &context.root, &paths_for_commit) {
                Ok(created) => created,
                Err(error) => {
                    eprintln!(
                        "could not create repository skill commit; changes remain uncommitted: {error:#}"
                    );
                    false
                }
            }
        }
        Some(Err(error)) => {
            eprintln!(
                "could not create repository skill commit; changes remain uncommitted: {error:#}"
            );
            false
        }
        None => false,
    };
    if commit_created {
        eprintln!("created repository skill commit");
    } else if changes_needed && is_maintainer != Some(true) {
        eprintln!("no commit created because the current account is not a repository maintainer");
    }
    let after = agent_guidance::status(&context.root)?;
    if !before.installed {
        eprintln!(
            "installed ngit repository skill version {}",
            after.bundled_version
        );
    } else if before.update_available {
        eprintln!(
            "upgraded ngit repository skill from {} to {}",
            before.installed_version.as_deref().unwrap_or("unknown"),
            after.bundled_version
        );
    } else if changes_needed {
        eprintln!(
            "reconciled ngit repository skill at bundled version {}",
            after.bundled_version
        );
    } else {
        eprintln!(
            "ngit repository skill is already at bundled version {}",
            after.bundled_version
        );
    }
    Ok(())
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

async fn resolve_maintainer(context: &SkillContext) -> Option<bool> {
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
    let account = get_likely_logged_in_user(&context.root).await.ok()??;
    Some(repo_ref.maintainers.contains(&account))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

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

    #[test]
    fn guidance_commit_updates_the_real_index() {
        let (repo, root) = repository();
        let paths = agent_guidance::paths_for_commit(&root).unwrap();
        agent_guidance::setup(&root, false).unwrap();

        assert!(agent_guidance::commit_guidance(&repo, &root, &paths).unwrap());

        let mut index = repo.git_repo.index().unwrap();
        let head = repo.git_repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(index.write_tree().unwrap(), head.tree_id());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_guidance_commit_restores_the_index_and_leaves_changes() {
        let (repo, root) = repository();
        let original_head = repo.git_repo.head().unwrap().peel_to_commit().unwrap();
        let head_name = repo.git_repo.head().unwrap().name().unwrap().to_owned();
        let paths = agent_guidance::paths_for_commit(&root).unwrap();
        agent_guidance::setup(&root, false).unwrap();
        let lock_path = repo.git_repo.path().join(format!("{head_name}.lock"));
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "locked\n").unwrap();

        assert!(agent_guidance::commit_guidance(&repo, &root, &paths).is_err());

        let reopened = git2::Repository::open(&root).unwrap();
        let head = reopened.head().unwrap().peel_to_commit().unwrap();
        let mut index = reopened.index().unwrap();
        assert_eq!(head.id(), original_head.id());
        assert_eq!(index.write_tree().unwrap(), original_head.tree_id());
        assert!(
            reopened
                .status_file(std::path::Path::new(agent_guidance::SKILL_PATH))
                .unwrap()
                .contains(git2::Status::WT_NEW)
        );
        fs::remove_file(lock_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guidance_commit_refuses_a_dirty_index() {
        let (repo, root) = repository();
        let paths = agent_guidance::paths_for_commit(&root).unwrap();
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        let mut index = repo.git_repo.index().unwrap();
        index.add_path(std::path::Path::new("staged.txt")).unwrap();
        index.write().unwrap();

        assert!(
            agent_guidance::preflight_dedicated_commit(&repo, &root, &paths)
                .unwrap_err()
                .to_string()
                .contains("index contains changes")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintainer_install_leaves_changes_when_index_is_dirty() {
        let (repo, root) = repository();
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        let mut index = repo.git_repo.index().unwrap();
        index.add_path(std::path::Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        let context = SkillContext {
            repo,
            root: root.clone(),
        };

        reconcile_for_account(&context, false, Some(true)).unwrap();

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
}
