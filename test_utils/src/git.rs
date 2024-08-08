//create

// implement drop?
use std::{
    env::current_dir,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use git2::{Branch, Oid, RepositoryInitOptions, Signature, Time};
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{Kind, ToBech32};

use crate::generate_repo_ref_event;

pub struct GitTestRepo {
    pub dir: PathBuf,
    pub git_repo: git2::Repository,
    pub delete_dir_on_drop: bool,
}

impl Default for GitTestRepo {
    fn default() -> Self {
        let repo_event = generate_repo_ref_event();
        let coordinate = Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: repo_event.author(),
            identifier: repo_event.identifier().unwrap().to_string(),
            relays: vec![
                "ws://localhost:8055".to_string(),
                "ws://localhost:8056".to_string(),
            ],
        };

        let repo = Self::new("main").unwrap();
        let _ = repo
            .git_repo
            .config()
            .unwrap()
            .set_str("nostr.repo", &coordinate.to_bech32().unwrap());
        repo
    }
}
impl GitTestRepo {
    pub fn new(main_branch_name: &str) -> Result<Self> {
        let path = current_dir()?.join(format!("tmpgit-{}", rand::random::<u64>()));
        let git_repo = git2::Repository::init_opts(
            &path,
            RepositoryInitOptions::new()
                .initial_head(main_branch_name)
                .mkpath(true),
        )?;
        // Make sure we have standard diffs for the tests so that user-level config does
        // not make them fail.
        git_repo.config()?.set_bool("diff.mnemonicPrefix", false)?;
        Ok(Self {
            dir: path,
            git_repo,
            delete_dir_on_drop: true,
        })
    }
    pub fn without_repo_in_git_config() -> Self {
        Self::new("main").unwrap()
    }

    pub fn open(path: &PathBuf) -> Result<Self> {
        let git_repo = git2::Repository::open(path)?;
        Ok(Self {
            dir: path.clone(),
            git_repo,
            delete_dir_on_drop: true,
        })
    }

    pub fn duplicate(existing_repo: &GitTestRepo) -> Result<Self> {
        let path = current_dir()?.join(format!("tmpgit-{}", rand::random::<u64>()));
        // function source: https://stackoverflow.com/a/65192210
        fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
            fs::create_dir_all(&dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                let ty = entry.file_type()?;
                if ty.is_dir() {
                    copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
                } else {
                    fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
                }
            }
            Ok(())
        }
        copy_dir_all(existing_repo.dir.clone(), path.clone())?;
        let git_repo = git2::Repository::open(path.clone())?;

        // let git_repo = git2::Repository::clone(existing_repo.dir.to_str().unwrap(),
        // path.clone())?;
        Ok(Self {
            dir: path,
            git_repo,
            delete_dir_on_drop: true,
        })
    }

    pub fn recreate_as_bare(existing_repo: &GitTestRepo) -> Result<Self> {
        // create bare repo
        let path = current_dir()?.join(format!("tmpgit-{}", rand::random::<u64>()));
        let git_repo = git2::Repository::init_opts(
            &path,
            RepositoryInitOptions::new()
                .initial_head("main")
                .bare(true)
                .mkpath(true),
        )?;
        // clone existing to a temp repo
        let tmp_repo = Self::duplicate(existing_repo)?;
        // add bare as a remote and push branches
        let mut remote = tmp_repo.git_repo.remote("tmp", path.to_str().unwrap())?;
        let refspecs = tmp_repo
            .git_repo
            .branches(Some(git2::BranchType::Local))?
            .filter_map(|b| b.ok())
            .map(|(b, _)| {
                format!(
                    "refs/heads/{}:refs/heads/{}",
                    b.name().unwrap().unwrap(),
                    b.name().unwrap().unwrap(),
                )
            })
            .collect::<Vec<String>>();
        remote.push(&refspecs, None)?;
        // TODO: push tags
        Ok(Self {
            dir: path,
            git_repo,
            delete_dir_on_drop: true,
        })
    }

    pub fn clone_repo(existing_repo: &GitTestRepo) -> Result<Self> {
        let path = current_dir()?.join(format!("tmpgit-{}", rand::random::<u64>()));
        let git_repo = git2::Repository::clone(existing_repo.dir.to_str().unwrap(), path.clone())?;
        Ok(Self {
            dir: path,
            git_repo,
            delete_dir_on_drop: true,
        })
    }

    pub fn initial_commit(&self) -> Result<Oid> {
        let oid = self.git_repo.index()?.write_tree()?;
        let tree = self.git_repo.find_tree(oid)?;
        let commit_oid = self.git_repo.commit(
            Some("HEAD"),
            &joe_signature(),
            &joe_signature(),
            "Initial commit",
            &tree,
            &[],
        )?;
        Ok(commit_oid)
    }

    pub fn populate(&self) -> Result<Oid> {
        self.initial_commit()?;
        fs::write(self.dir.join("t1.md"), "some content")?;
        self.stage_and_commit("add t1.md")?;
        fs::write(self.dir.join("t2.md"), "some content1")?;
        self.stage_and_commit("add t2.md")
    }

    pub fn populate_minus_1(&self) -> Result<Oid> {
        self.initial_commit()?;
        fs::write(self.dir.join("t1.md"), "some content")?;
        self.stage_and_commit("add t1.md")
    }

    pub fn populate_with_test_branch(&self) -> Result<Oid> {
        self.populate()?;
        self.create_branch("add-example-feature")?;
        self.checkout("add-example-feature")?;
        fs::write(self.dir.join("f1.md"), "some content")?;
        self.stage_and_commit("add f1.md")?;
        fs::write(self.dir.join("f2.md"), "some content")?;
        self.stage_and_commit("add f2.md")?;
        fs::write(self.dir.join("f3.md"), "some content1")?;
        self.stage_and_commit("add f3.md")
    }

    pub fn stage_and_commit(&self, message: &str) -> Result<Oid> {
        self.stage_and_commit_custom_signature(message, None, None)
    }

    pub fn stage_and_commit_custom_signature(
        &self,
        message: &str,
        author: Option<&git2::Signature>,
        commiter: Option<&git2::Signature>,
    ) -> Result<Oid> {
        let prev_oid = self.git_repo.head().unwrap().peel_to_commit()?;

        let mut index = self.git_repo.index()?;
        index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;

        let oid = self.git_repo.commit(
            Some("HEAD"),
            author.unwrap_or(&joe_signature()),
            commiter.unwrap_or(&joe_signature()),
            message,
            &self.git_repo.find_tree(index.write_tree()?)?,
            &[&prev_oid],
        )?;

        Ok(oid)
    }

    pub fn create_branch(&self, branch_name: &str) -> Result<Branch> {
        self.git_repo
            .branch(branch_name, &self.git_repo.head()?.peel_to_commit()?, false)
            .context("could not create branch")
    }

    pub fn checkout(&self, ref_name: &str) -> Result<Oid> {
        let (object, reference) = self.git_repo.revparse_ext(ref_name)?;

        self.git_repo.checkout_tree(&object, None)?;

        match reference {
            // gref is an actual reference like branches or tags
            Some(gref) => self.git_repo.set_head(gref.name().unwrap()),
            // this is a commit, not a reference
            None => self.git_repo.set_head_detached(object.id()),
        }?;
        let oid = self.git_repo.head()?.peel_to_commit()?.id();
        Ok(oid)
    }

    pub fn get_local_branch_names(&self) -> Result<Vec<String>> {
        let local_branches = self
            .git_repo
            .branches(Some(git2::BranchType::Local))
            .context("getting GitRepo branches should not error even for a blank repository")?;

        let mut branch_names = vec![];

        for iter in local_branches {
            let branch = iter?.0;
            if let Some(name) = branch.name()? {
                branch_names.push(name.to_string());
            }
        }
        Ok(branch_names)
    }

    pub fn get_checked_out_branch_name(&self) -> Result<String> {
        Ok(self
            .git_repo
            .head()?
            .shorthand()
            .context("an object without a shorthand is checked out")?
            .to_string())
    }

    pub fn get_tip_of_local_branch(&self, branch_name: &str) -> Result<Oid> {
        let branch = self
            .git_repo
            .find_branch(branch_name, git2::BranchType::Local)
            .context(format!("cannot find branch {branch_name}"))?;
        Ok(branch.into_reference().peel_to_commit()?.id())
    }

    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.git_repo.remote(name, url)?;
        Ok(())
    }

    pub fn checkout_remote_branch(&self, branch_name: &str) -> Result<Oid> {
        self.checkout(&format!("remotes/origin/{branch_name}"))?;
        let mut branch = self.create_branch(branch_name)?;
        branch.set_upstream(Some(&format!("origin/{branch_name}")))?;
        self.checkout(branch_name)
    }
}

impl Drop for GitTestRepo {
    fn drop(&mut self) {
        if self.delete_dir_on_drop {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
pub fn joe_signature() -> Signature<'static> {
    Signature::new("Joe Bloggs", "joe.bloggs@pm.me", &Time::new(0, 0)).unwrap()
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn methods_do_not_throw() -> Result<()> {
        let repo = GitTestRepo::new("main")?;

        repo.populate()?;
        repo.create_branch("feature")?;
        repo.checkout("feature")?;
        fs::write(repo.dir.join("t3.md"), "some content")?;
        repo.stage_and_commit("add t3.md")?;

        Ok(())
    }
}
