use std::env::current_dir;
#[cfg(test)]
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use git2::{Oid, Revwalk};
use nostr::prelude::{sha1::Hash as Sha1Hash, Hash};

pub struct Repo {
    git_repo: git2::Repository,
}

impl Repo {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            git_repo: git2::Repository::discover(current_dir()?)?,
        })
    }
    #[cfg(test)]
    pub fn from_path(path: &PathBuf) -> Result<Self> {
        Ok(Self {
            git_repo: git2::Repository::open(path)?,
        })
    }
}

// pub type CommitId = [u8; 7];
// pub type Sha1 = [u8; 20];

pub trait RepoActions {
    fn get_local_branch_names(&self) -> Result<Vec<String>>;
    fn get_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)>;
    fn get_tip_of_local_branch(&self, branch_name: &str) -> Result<Sha1Hash>;
    fn get_root_commit(&self, branch_name: &str) -> Result<Sha1Hash>;
    fn get_head_commit(&self) -> Result<Sha1Hash>;
    fn get_commit_parent(&self, commit: &Sha1Hash) -> Result<Sha1Hash>;
    fn get_commits_ahead_behind(
        &self,
        base_commit: &Sha1Hash,
        latest_commit: &Sha1Hash,
    ) -> Result<(Vec<Sha1Hash>, Vec<Sha1Hash>)>;
    fn make_patch_from_commit(&self, commit: &Sha1Hash) -> Result<String>;
}

impl RepoActions for Repo {
    fn get_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)> {
        let main_branch_name = {
            let local_branches = self
                .get_local_branch_names()
                .context("cannot find any local branches")?;
            if local_branches.contains(&"main".to_string()) {
                "main"
            } else if local_branches.contains(&"master".to_string()) {
                "master"
            } else {
                bail!("no main or master branch locally in this git repository to initiate from",)
            }
        };

        let tip = self
            .get_tip_of_local_branch(main_branch_name)
            .context(format!(
                "branch {main_branch_name} was listed as a local branch but cannot get its tip commit id",
            ))?;

        Ok((main_branch_name, tip))
    }

    fn get_local_branch_names(&self) -> Result<Vec<String>> {
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

    fn get_tip_of_local_branch(&self, branch_name: &str) -> Result<Sha1Hash> {
        let branch = self
            .git_repo
            .find_branch(branch_name, git2::BranchType::Local)
            .context(format!("cannot find branch {branch_name}"))?;
        Ok(oid_to_sha1(&branch.into_reference().peel_to_commit()?.id()))
    }

    fn get_root_commit(&self, branch_name: &str) -> Result<Sha1Hash> {
        let tip = self.get_tip_of_local_branch(branch_name)?;
        let mut revwalk = self
            .git_repo
            .revwalk()
            .context("revwalk should be created from git repo")?;
        revwalk
            .push(sha1_to_oid(&tip)?)
            .context("revwalk should accept tip oid")?;
        Ok(oid_to_sha1(
            &revwalk
                .last()
                .context("revwalk from tip should be at least contain the tip oid")?
                .context("revwalk iter from branch tip should not result in an error")?,
        ))
    }

    fn get_head_commit(&self) -> Result<Sha1Hash> {
        let head = self
            .git_repo
            .head()
            .context("failed to get git repo head")?;
        let oid = head.peel_to_commit()?.id();
        Ok(oid_to_sha1(&oid))
    }

    fn get_commit_parent(&self, commit: &Sha1Hash) -> Result<Sha1Hash> {
        let parent_oid = self
            .git_repo
            .find_commit(sha1_to_oid(commit)?)
            .context(format!("could not find commit {commit}"))?
            .parent_id(0)
            .context(format!("could not find parent of commit {commit}"))?;
        Ok(oid_to_sha1(&parent_oid))
    }

    fn make_patch_from_commit(&self, commit: &Sha1Hash) -> Result<String> {
        let c = self
            .git_repo
            .find_commit(Oid::from_bytes(commit.as_byte_array()).context(format!(
                "failed to convert commit_id format for {}",
                &commit
            ))?)
            .context(format!("failed to find commit {}", &commit))?;
        let patch = git2::Email::from_commit(&c, &mut git2::EmailCreateOptions::default())
            .context(format!("failed to create patch from commit {}", &commit))?;

        Ok(std::str::from_utf8(patch.as_slice())
            .context("patch content could not be converted to a utf8 string")?
            .to_owned())
    }

    fn get_commits_ahead_behind(
        &self,
        base_commit: &Sha1Hash,
        latest_commit: &Sha1Hash,
    ) -> Result<(Vec<Sha1Hash>, Vec<Sha1Hash>)> {
        let mut ahead: Vec<Sha1Hash> = vec![];
        let mut behind: Vec<Sha1Hash> = vec![];

        let get_revwalk = |commit: &Sha1Hash| -> Result<Revwalk> {
            let mut revwalk = self
                .git_repo
                .revwalk()
                .context("revwalk should be created from git repo")?;
            revwalk
                .push(sha1_to_oid(commit)?)
                .context("revwalk should accept commit oid")?;
            Ok(revwalk)
        };

        // scan through the base commit ancestory until a common ancestor is found
        let most_recent_shared_commit = match get_revwalk(base_commit)
            .context("failed to get revwalk for base_commit")?
            .find(|base_res| {
                let base_oid = base_res.as_ref().unwrap();

                if get_revwalk(latest_commit)
                    .unwrap()
                    .any(|latest_res| base_oid.eq(latest_res.as_ref().unwrap()))
                {
                    true
                } else {
                    // add commits not found in latest ancestory to 'behind' vector
                    behind.push(oid_to_sha1(base_oid));
                    false
                }
            }) {
            None => {
                bail!(format!(
                    "{} is not an ancestor of {}",
                    latest_commit, base_commit
                ));
            }
            Some(res) => res.context("revwalk failed to reveal commit")?,
        };

        // scan through the latest commits until shared commit is reached
        get_revwalk(latest_commit)
            .context("failed to get revwalk for latest_commit")?
            .any(|latest_res| {
                let latest_oid = latest_res.as_ref().unwrap();
                if latest_oid.eq(&most_recent_shared_commit) {
                    true
                } else {
                    // add commits not found in base to 'ahead' vector
                    ahead.push(oid_to_sha1(latest_oid));
                    false
                }
            });
        Ok((ahead, behind))
    }
}

fn oid_to_u8_20_bytes(oid: &Oid) -> [u8; 20] {
    let b = oid.as_bytes();
    [
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15], b[16], b[17], b[18], b[19],
    ]
}

// fn oid_to_shorthand_string(oid: Oid) -> Result<String> {
//     let binding = oid.to_string();
//     let b = binding.as_bytes();
//     String::from_utf8(vec![b[0], b[1], b[2], b[3], b[4], b[5], b[6]])
//         .context("oid should always start with 7 u8 btyes of utf8")
// }

// fn oid_to_sha1_string(oid: Oid) -> Result<String> {
//     let b = oid.as_bytes();
//     String::from_utf8(vec![
//         b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10],
// b[11], b[12], b[13],         b[14], b[15], b[16], b[17], b[18], b[19],
//     ])
//     .context("oid should contain 20 u8 btyes of utf8")
// }

// git2 Oid object to Sha1Hash
pub fn oid_to_sha1(oid: &Oid) -> Sha1Hash {
    Sha1Hash::from_byte_array(oid_to_u8_20_bytes(oid))
}

/// `Sha1Hash` to git2 `Oid` object
fn sha1_to_oid(hash: &Sha1Hash) -> Result<Oid> {
    Oid::from_bytes(hash.as_byte_array()).context("Sha1Hash bytes failed to produce a valid Oid")
}

#[cfg(test)]
mod tests {
    use test_utils::git::GitTestRepo;

    use super::*;

    #[test]
    fn get_commit_parent() -> Result<()> {
        let test_repo = GitTestRepo::default();
        let parent_oid = test_repo.populate()?;
        std::fs::write(test_repo.dir.join("t100.md"), "some content")?;
        let child_oid = test_repo.stage_and_commit("add t100.md")?;

        let git_repo = Repo::from_path(&test_repo.dir)?;

        assert_eq!(
            // Sha1Hash::from_byte_array("bla".to_string().as_bytes()),
            oid_to_sha1(&parent_oid),
            git_repo.get_commit_parent(&oid_to_sha1(&child_oid))?,
        );
        Ok(())
    }

    mod make_patch_from_commit {
        use super::*;
        #[test]
        fn simple_patch_matches_string() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let oid = test_repo.populate()?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert_eq!(
                "\
                From 431b84edc0d2fa118d63faa3c2db9c73d630a5ae Mon Sep 17 00:00:00 2001\n\
                From: Joe Bloggs <joe.bloggs@pm.me>\n\
                Date: Thu, 1 Jan 1970 00:00:00 +0000\n\
                Subject: [PATCH] add t2.md\n\
                \n\
                ---\n \
                t2.md | 1 +\n \
                1 file changed, 1 insertion(+)\n \
                create mode 100644 t2.md\n\
                \n\
                diff --git a/t2.md b/t2.md\n\
                new file mode 100644\n\
                index 0000000..a66525d\n\
                --- /dev/null\n\
                +++ b/t2.md\n\
                @@ -0,0 +1 @@\n\
                +some content1\n\\ \
                No newline at end of file\n\
                --\n\
                libgit2 1.7.1\n\
                \n\
                ",
                git_repo.make_patch_from_commit(&oid_to_sha1(&oid))?,
            );
            Ok(())
        }
    }

    mod get_main_or_master_branch {

        use super::*;
        mod returns_main {
            use super::*;
            #[test]
            fn when_it_exists() -> Result<()> {
                let test_repo = GitTestRepo::new("main")?;
                let main_oid = test_repo.populate()?;
                let git_repo = Repo::from_path(&test_repo.dir)?;
                let (name, commit_hash) = git_repo.get_main_or_master_branch()?;
                assert_eq!(name, "main");
                assert_eq!(commit_hash, oid_to_sha1(&main_oid));
                Ok(())
            }

            #[test]
            fn when_it_exists_and_other_branch_checkedout() -> Result<()> {
                let test_repo = GitTestRepo::new("main")?;
                let main_oid = test_repo.populate()?;
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let feature_oid = test_repo.stage_and_commit("add t3.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;
                let (name, commit_hash) = git_repo.get_main_or_master_branch()?;
                assert_eq!(name, "main");
                assert_eq!(commit_hash, oid_to_sha1(&main_oid));
                assert_ne!(commit_hash, oid_to_sha1(&feature_oid));
                Ok(())
            }

            #[test]
            fn when_exists_even_if_master_is_checkedout() -> Result<()> {
                let test_repo = GitTestRepo::new("main")?;
                let main_oid = test_repo.populate()?;
                test_repo.create_branch("master")?;
                test_repo.checkout("master")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let master_oid = test_repo.stage_and_commit("add t3.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;
                let (name, commit_hash) = git_repo.get_main_or_master_branch()?;
                assert_eq!(name, "main");
                assert_eq!(commit_hash, oid_to_sha1(&main_oid));
                assert_ne!(commit_hash, oid_to_sha1(&master_oid));
                Ok(())
            }
        }

        #[test]
        fn returns_master_if_exists_and_main_doesnt() -> Result<()> {
            let test_repo = GitTestRepo::new("master")?;
            let master_oid = test_repo.populate()?;
            test_repo.create_branch("feature")?;
            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let feature_oid = test_repo.stage_and_commit("add t3.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;
            let (name, commit_hash) = git_repo.get_main_or_master_branch()?;
            assert_eq!(name, "master");
            assert_eq!(commit_hash, oid_to_sha1(&master_oid));
            assert_ne!(commit_hash, oid_to_sha1(&feature_oid));
            Ok(())
        }
        #[test]
        fn returns_error_if_no_main_or_master() -> Result<()> {
            let test_repo = GitTestRepo::new("feature")?;
            test_repo.populate()?;
            let git_repo = Repo::from_path(&test_repo.dir)?;
            assert!(git_repo.get_main_or_master_branch().is_err());
            Ok(())
        }
    }

    mod get_commits_ahead_behind {
        use super::*;
        mod returns_main {
            use super::*;

            #[test]
            fn when_on_same_commit_return_empty() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let oid = test_repo.populate()?;
                // create feature branch
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let (ahead, behind) =
                    git_repo.get_commits_ahead_behind(&oid_to_sha1(&oid), &oid_to_sha1(&oid))?;
                assert_eq!(ahead, vec![]);
                assert_eq!(behind, vec![]);
                Ok(())
            }

            #[test]
            fn when_2_commit_behind() -> Result<()> {
                let test_repo = GitTestRepo::default();
                test_repo.populate()?;
                // create feature branch
                test_repo.create_branch("feature")?;
                let feature_oid = test_repo.checkout("feature")?;
                // checkout main and add 2 commits
                test_repo.checkout("main")?;
                std::fs::write(test_repo.dir.join("t5.md"), "some content")?;
                let behind_1_oid = test_repo.stage_and_commit("add t5.md")?;
                std::fs::write(test_repo.dir.join("t6.md"), "some content")?;
                let behind_2_oid = test_repo.stage_and_commit("add t6.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let (ahead, behind) = git_repo.get_commits_ahead_behind(
                    &oid_to_sha1(&behind_2_oid),
                    &oid_to_sha1(&feature_oid),
                )?;
                assert_eq!(ahead, vec![]);
                assert_eq!(
                    behind,
                    vec![oid_to_sha1(&behind_2_oid), oid_to_sha1(&behind_1_oid),],
                );
                Ok(())
            }

            #[test]
            fn when_2_commit_ahead() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let main_oid = test_repo.populate()?;
                // create feature branch and add 2 commits
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
                std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
                let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let (ahead, behind) = git_repo.get_commits_ahead_behind(
                    &oid_to_sha1(&main_oid),
                    &oid_to_sha1(&ahead_2_oid),
                )?;
                assert_eq!(
                    ahead,
                    vec![oid_to_sha1(&ahead_2_oid), oid_to_sha1(&ahead_1_oid),],
                );
                assert_eq!(behind, vec![]);
                Ok(())
            }

            #[test]
            fn when_2_commit_ahead_and_2_commits_behind() -> Result<()> {
                let test_repo = GitTestRepo::default();
                test_repo.populate()?;
                // create feature branch and add 2 commits
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
                std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
                let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;
                // checkout main and add 2 commits
                test_repo.checkout("main")?;
                std::fs::write(test_repo.dir.join("t5.md"), "some content")?;
                let behind_1_oid = test_repo.stage_and_commit("add t5.md")?;
                std::fs::write(test_repo.dir.join("t6.md"), "some content")?;
                let behind_2_oid = test_repo.stage_and_commit("add t6.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let (ahead, behind) = git_repo.get_commits_ahead_behind(
                    &oid_to_sha1(&behind_2_oid),
                    &oid_to_sha1(&ahead_2_oid),
                )?;
                assert_eq!(
                    ahead,
                    vec![oid_to_sha1(&ahead_2_oid), oid_to_sha1(&ahead_1_oid)],
                );
                assert_eq!(
                    behind,
                    vec![oid_to_sha1(&behind_2_oid), oid_to_sha1(&behind_1_oid)],
                );
                Ok(())
            }
        }
    }
}
