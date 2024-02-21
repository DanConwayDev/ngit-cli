//create

// implement drop?
use std::{env::current_dir, fs, path::PathBuf};

use anyhow::{Context, Result};
use git2::{Oid, RepositoryInitOptions, Signature, Time};

pub struct GitTestRepo {
    pub dir: PathBuf,
    pub git_repo: git2::Repository,
}

impl Default for GitTestRepo {
    fn default() -> Self {
        Self::new("main").unwrap()
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
        Ok(Self {
            dir: path,
            git_repo,
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

    pub fn create_branch(&self, branch_name: &str) -> Result<()> {
        self.git_repo
            .branch(branch_name, &self.git_repo.head()?.peel_to_commit()?, false)?;
        Ok(())
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
}

impl Drop for GitTestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
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
