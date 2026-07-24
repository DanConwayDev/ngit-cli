use anyhow::{Context, Result};
use ngit::{
    agent_guidance,
    client::get_repo_ref_from_cache,
    git::{Repo, RepoActions},
    login::get_likely_logged_in_user,
};
use serde::Serialize;

use crate::cli::AgentCommands;

struct AgentContext {
    repo: Repo,
    root: std::path::PathBuf,
}

#[derive(Serialize)]
struct Output {
    #[serde(flatten)]
    guidance: agent_guidance::GuidanceStatus,
    is_maintainer: Option<bool>,
}

pub async fn launch(command: &AgentCommands) -> Result<()> {
    let context = resolve_context()?;
    match command {
        AgentCommands::Setup { force } => {
            agent_guidance::setup(&context.root, *force)?;
            eprintln!("installed ngit agent guidance");
        }
        AgentCommands::Status { json } => {
            let output = Output {
                guidance: agent_guidance::status(&context.root)?,
                is_maintainer: resolve_maintainer(&context).await,
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
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
                    output.is_maintainer.map_or("unknown", |value| if value {
                        "true"
                    } else {
                        "false"
                    })
                );
                println!(
                    "managed files: {}",
                    output.guidance.managed_files.join(", ")
                );
            }
        }
        AgentCommands::Update {
            diff,
            commit,
            force,
        } => {
            if *diff {
                print!("{}", agent_guidance::proposed_diff(&context.root)?);
                return Ok(());
            }
            if *commit {
                agent_guidance::preflight_dedicated_commit(&context.repo, &context.root)?;
            }
            agent_guidance::update(&context.root, *force)?;
            if *commit && agent_guidance::commit_guidance(&context.repo, &context.root)? {
                eprintln!("created guidance commit");
            }
            eprintln!("updated ngit agent guidance");
        }
    }
    Ok(())
}

fn resolve_context() -> Result<AgentContext> {
    let repo = Repo::discover().context("ngit agent must run inside a Git working tree")?;
    let root = repo.get_path()?.to_path_buf();
    Ok(AgentContext { repo, root })
}

async fn resolve_maintainer(context: &AgentContext) -> Option<bool> {
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
            "ngit-agent-command-{}-{}",
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
        agent_guidance::setup(&root, false).unwrap();

        assert!(agent_guidance::commit_guidance(&repo, &root).unwrap());

        let mut index = repo.git_repo.index().unwrap();
        let head = repo.git_repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(index.write_tree().unwrap(), head.tree_id());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guidance_commit_refuses_a_dirty_index() {
        let (repo, root) = repository();
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        let mut index = repo.git_repo.index().unwrap();
        index.add_path(std::path::Path::new("staged.txt")).unwrap();
        index.write().unwrap();

        assert!(
            agent_guidance::preflight_dedicated_commit(&repo, &root)
                .unwrap_err()
                .to_string()
                .contains("index contains changes")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
