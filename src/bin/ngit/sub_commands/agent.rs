use anyhow::{Context, Result, bail};
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
    maintainer: bool,
}

#[derive(Serialize)]
struct Output {
    #[serde(flatten)]
    guidance: agent_guidance::GuidanceStatus,
    is_maintainer: bool,
}

pub async fn launch(command: &AgentCommands) -> Result<()> {
    let context = resolve_context().await?;
    match command {
        AgentCommands::Setup { force } => {
            require_maintainer(&context)?;
            agent_guidance::setup(&context.root, *force)?;
            eprintln!("installed ngit agent guidance");
        }
        AgentCommands::Status { json } => {
            let output = Output {
                guidance: agent_guidance::status(&context.root)?,
                is_maintainer: context.maintainer,
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
                println!("current account is maintainer: {}", output.is_maintainer);
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
            require_maintainer(&context)?;
            if *diff {
                print!("{}", agent_guidance::proposed_diff(&context.root)?);
                return Ok(());
            }
            if *commit {
                ensure_index_is_clean(&context.repo)?;
            }
            agent_guidance::update(&context.root, *force)?;
            if *commit && commit_guidance(&context.repo, &context.root)? {
                eprintln!("created guidance commit");
            }
            eprintln!("updated ngit agent guidance");
        }
    }
    Ok(())
}

async fn resolve_context() -> Result<AgentContext> {
    // Leak no data: only the public key stored alongside a login is needed.
    let repo = Repo::discover().context("ngit agent must run inside a Git working tree")?;
    let root = repo.get_path()?.to_path_buf();
    let (_, decoded) = repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await?
        .context("ngit agent requires a repository with a nostr:// remote")?;
    let repo_ref = get_repo_ref_from_cache(Some(&root), &decoded.coordinate)
        .await
        .context(
            "failed to resolve the Nostr repository announcement; fetch the nostr remote and retry",
        )?;
    let account = get_likely_logged_in_user(&root)
        .await
        .context("failed to resolve the logged-in account")?
        .context("no logged-in account found; run `ngit account login`")?;
    let maintainer = repo_ref.maintainers.contains(&account);
    Ok(AgentContext {
        repo,
        root,
        maintainer,
    })
}

fn require_maintainer(context: &AgentContext) -> Result<()> {
    if context.maintainer {
        Ok(())
    } else {
        bail!("the logged-in account is not a repository maintainer")
    }
}

fn commit_guidance(repo: &Repo, root: &std::path::Path) -> Result<bool> {
    ensure_index_is_clean(repo)?;
    if repo.merge_in_progress()? {
        bail!("cannot create a guidance commit while a merge is in progress")
    }
    let head = repo
        .git_repo
        .head()
        .context("cannot create a dedicated guidance commit without HEAD")?;
    let parent = head.peel_to_commit()?;
    let tree = parent.tree()?;
    // Start with the repository-backed index so `add_path` can read the
    // guidance files from the worktree. The caller has already verified this
    // index matches HEAD, so this does not absorb unrelated staged changes.
    let mut index = repo.git_repo.index()?;
    index.read_tree(&tree)?;
    for path in agent_guidance::paths_for_commit(root)? {
        let relative = path
            .strip_prefix(root)
            .context("guidance path outside worktree")?;
        index.add_path(relative)?;
    }
    let tree_id = index.write_tree_to(&repo.git_repo)?;
    if tree_id == tree.id() {
        return Ok(false);
    }
    let signature = repo
        .git_repo
        .signature()
        .context("cannot determine Git author for guidance commit")?;
    repo.git_repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "chore: update ngit agent guidance",
        &repo.git_repo.find_tree(tree_id)?,
        &[&parent],
    )?;
    index.write()?;
    Ok(true)
}

fn ensure_index_is_clean(repo: &Repo) -> Result<()> {
    let head = repo
        .git_repo
        .head()
        .context("cannot create a dedicated guidance commit without HEAD")?
        .peel_to_commit()?;
    let mut index = repo.git_repo.index()?;
    if index.has_conflicts() || index.write_tree()? != head.tree_id() {
        bail!("cannot create a guidance commit while the Git index contains changes")
    }
    Ok(())
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

        assert!(commit_guidance(&repo, &root).unwrap());

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
            ensure_index_is_clean(&repo)
                .unwrap_err()
                .to_string()
                .contains("index contains changes")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
