#[cfg(test)]
use std::path::PathBuf;
use std::{env::current_dir, path::Path};

use anyhow::{bail, Context, Result};
use git2::{DiffOptions, Oid, Revwalk};
use nostr_sdk::hashes::{sha1::Hash as Sha1Hash, Hash};

use crate::sub_commands::list::{get_commit_id_from_patch, tag_value};

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
    fn get_path(&self) -> Result<&Path>;
    fn get_origin_url(&self) -> Result<String>;
    fn get_remote_branch_names(&self) -> Result<Vec<String>>;
    fn get_local_branch_names(&self) -> Result<Vec<String>>;
    fn get_origin_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)>;
    fn get_local_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)>;
    fn get_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)>;
    fn get_checked_out_branch_name(&self) -> Result<String>;
    fn get_tip_of_branch(&self, branch_name: &str) -> Result<Sha1Hash>;
    fn get_root_commit(&self) -> Result<Sha1Hash>;
    fn does_commit_exist(&self, commit: &str) -> Result<bool>;
    fn get_head_commit(&self) -> Result<Sha1Hash>;
    fn get_commit_parent(&self, commit: &Sha1Hash) -> Result<Sha1Hash>;
    fn get_commit_message(&self, commit: &Sha1Hash) -> Result<String>;
    fn get_commit_message_summary(&self, commit: &Sha1Hash) -> Result<String>;
    #[allow(clippy::doc_link_with_quotes)]
    /// returns vector ["name", "email", "unixtime", "offset"]
    /// eg ["joe bloggs", "joe@pm.me", "12176","-300"]
    fn get_commit_author(&self, commit: &Sha1Hash) -> Result<Vec<String>>;
    #[allow(clippy::doc_link_with_quotes)]
    /// returns vector ["name", "email", "unixtime", "offset"]
    /// eg ["joe bloggs", "joe@pm.me", "12176","-300"]
    fn get_commit_comitter(&self, commit: &Sha1Hash) -> Result<Vec<String>>;
    fn get_commits_ahead_behind(
        &self,
        base_commit: &Sha1Hash,
        latest_commit: &Sha1Hash,
    ) -> Result<(Vec<Sha1Hash>, Vec<Sha1Hash>)>;
    fn get_refs(&self, commit: &Sha1Hash) -> Result<Vec<String>>;
    // including (un)staged changes and (un)tracked files
    fn has_outstanding_changes(&self) -> Result<bool>;
    fn make_patch_from_commit(
        &self,
        commit: &Sha1Hash,
        series_count: &Option<(u64, u64)>,
    ) -> Result<String>;
    fn extract_commit_pgp_signature(&self, commit: &Sha1Hash) -> Result<String>;
    fn checkout(&self, ref_name: &str) -> Result<Sha1Hash>;
    fn create_branch_at_commit(&self, branch_name: &str, commit: &str) -> Result<()>;
    fn apply_patch_chain(
        &self,
        branch_name: &str,
        patch_and_ancestors: Vec<nostr::Event>,
    ) -> Result<Vec<nostr::Event>>;
    fn parse_starting_commits(&self, starting_commits: &str) -> Result<Vec<Sha1Hash>>;
    fn ancestor_of(&self, decendant: &Sha1Hash, ancestor: &Sha1Hash) -> Result<bool>;
}

impl RepoActions for Repo {
    fn get_path(&self) -> Result<&Path> {
        self.git_repo
            .path()
            .parent()
            .context("cannot find repositiory path as .git has  no parent")
    }

    fn get_origin_url(&self) -> Result<String> {
        Ok(self
            .git_repo
            .find_remote("origin")
            .context("cannot find origin")?
            .url()
            .context("cannot find origin url")?
            .to_string())
    }

    fn get_origin_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)> {
        let main_branch_name = {
            let remote_branches = self
                .get_remote_branch_names()
                .context("cannot find any local branches")?;
            if remote_branches.contains(&"origin/main".to_string()) {
                "origin/main"
            } else if remote_branches.contains(&"origin/master".to_string()) {
                "origin/master"
            } else {
                bail!("no main or master branch locally in this git repository to initiate from",)
            }
        };

        let tip = self
            .get_tip_of_branch(main_branch_name)
            .context(format!(
                "branch {main_branch_name} was listed as a remote branch but cannot get its tip commit id",
            ))?;

        Ok((main_branch_name, tip))
    }

    fn get_local_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)> {
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
            .get_tip_of_branch(main_branch_name)
            .context(format!(
                "branch {main_branch_name} was listed as a local branch but cannot get its tip commit id",
            ))?;

        Ok((main_branch_name, tip))
    }

    fn get_main_or_master_branch(&self) -> Result<(&str, Sha1Hash)> {
        if let Ok(main_tuple) = self
            .get_origin_main_or_master_branch()
            .context("the default branches (main or master) do not exist")
        {
            Ok(main_tuple)
        } else {
            self.get_local_main_or_master_branch()
                .context("the default branches (main or master) do not exist")
        }
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

    fn get_remote_branch_names(&self) -> Result<Vec<String>> {
        let remote_branches = self
            .git_repo
            .branches(Some(git2::BranchType::Remote))
            .context("getting GitRepo branches should not error even for a blank repository")?;

        let mut branch_names = vec![];

        for iter in remote_branches {
            let branch = iter?.0;
            if let Some(name) = branch.name()? {
                branch_names.push(name.to_string());
            }
        }
        Ok(branch_names)
    }

    fn get_checked_out_branch_name(&self) -> Result<String> {
        Ok(self
            .git_repo
            .head()?
            .shorthand()
            .context("an object without a shorthand is checked out")?
            .to_string())
    }

    fn get_tip_of_branch(&self, branch_name: &str) -> Result<Sha1Hash> {
        let branch = if let Ok(branch) = self
            .git_repo
            .find_branch(branch_name, git2::BranchType::Local)
            .context(format!("cannot find local branch {branch_name}"))
        {
            branch
        } else {
            self.git_repo
                .find_branch(branch_name, git2::BranchType::Remote)
                .context(format!("cannot find local or remote branch {branch_name}"))?
        };
        Ok(oid_to_sha1(&branch.into_reference().peel_to_commit()?.id()))
    }

    fn get_root_commit(&self) -> Result<Sha1Hash> {
        let mut revwalk = self
            .git_repo
            .revwalk()
            .context("revwalk should be created from git repo")?;
        revwalk
            .push(sha1_to_oid(&self.get_head_commit()?)?)
            .context("revwalk should accept tip oid")?;
        Ok(oid_to_sha1(
            &revwalk
                .last()
                .context("revwalk from tip should be at least contain the tip oid")?
                .context("revwalk iter from branch tip should not result in an error")?,
        ))
    }

    fn does_commit_exist(&self, commit: &str) -> Result<bool> {
        if self.git_repo.find_commit(Oid::from_str(commit)?).is_ok() {
            Ok(true)
        } else {
            Ok(false)
        }
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

    fn get_commit_message(&self, commit: &Sha1Hash) -> Result<String> {
        Ok(self
            .git_repo
            .find_commit(sha1_to_oid(commit)?)
            .context(format!("could not find commit {commit}"))?
            .message_raw()
            .context("commit message has unusual characters in (not valid utf-8)")?
            .to_string())
    }

    fn get_commit_message_summary(&self, commit: &Sha1Hash) -> Result<String> {
        Ok(self
            .git_repo
            .find_commit(sha1_to_oid(commit)?)
            .context(format!("could not find commit {commit}"))?
            .message_raw()
            .context("commit message has unusual characters in (not valid utf-8)")?
            .split('\r')
            .collect::<Vec<&str>>()[0]
            .split('\n')
            .collect::<Vec<&str>>()[0]
            .to_string()
            .trim()
            .to_string())
    }

    fn get_commit_author(&self, commit: &Sha1Hash) -> Result<Vec<String>> {
        let commit = self
            .git_repo
            .find_commit(sha1_to_oid(commit)?)
            .context(format!("could not find commit {commit}"))?;
        let sig = commit.author();
        Ok(git_sig_to_tag_vec(&sig))
    }

    fn get_commit_comitter(&self, commit: &Sha1Hash) -> Result<Vec<String>> {
        let commit = self
            .git_repo
            .find_commit(sha1_to_oid(commit)?)
            .context(format!("could not find commit {commit}"))?;
        let sig = commit.committer();
        Ok(git_sig_to_tag_vec(&sig))
    }

    fn get_refs(&self, commit: &Sha1Hash) -> Result<Vec<String>> {
        Ok(self
            .git_repo
            .references()?
            .filter(|r| {
                if let Ok(r) = r {
                    if let Ok(ref_tip) = r.peel_to_commit() {
                        ref_tip.id().to_string().eq(&commit.to_string())
                    } else {
                        false
                    }
                } else {
                    false
                }
            })
            .map(|r| r.unwrap().shorthand().unwrap().to_string())
            .collect::<Vec<String>>())
    }

    fn make_patch_from_commit(
        &self,
        commit: &Sha1Hash,
        series_count: &Option<(u64, u64)>,
    ) -> Result<String> {
        let c = self
            .git_repo
            .find_commit(Oid::from_bytes(commit.as_byte_array()).context(format!(
                "failed to convert commit_id format for {}",
                &commit
            ))?)
            .context(format!("failed to find commit {}", &commit))?;
        let mut options = git2::EmailCreateOptions::default();
        if let Some((n, total)) = series_count {
            options.subject_prefix(format!("PATCH {n}/{total}"));
        }
        let patch = git2::Email::from_commit(&c, &mut options)
            .context(format!("failed to create patch from commit {}", &commit))?;

        Ok(std::str::from_utf8(patch.as_slice())
            .context("patch content could not be converted to a utf8 string")?
            .to_owned())
    }

    fn extract_commit_pgp_signature(&self, commit: &Sha1Hash) -> Result<String> {
        let oid = Oid::from_bytes(commit.as_byte_array()).context(format!(
            "failed to convert commit_id format for {}",
            &commit
        ))?;

        let (sign, _data) = self
            .git_repo
            .extract_signature(&oid, None)
            .context("failed to extract signature - perhaps there is no signature?")?;

        Ok(std::str::from_utf8(&sign)
            .context("commit signature cannot be converted to a utf8 string")?
            .to_owned())
    }

    // including (un)staged changes and (un)tracked files
    fn has_outstanding_changes(&self) -> Result<bool> {
        let diff = self.git_repo.diff_tree_to_workdir_with_index(
            Some(&self.git_repo.head()?.peel_to_tree()?),
            Some(DiffOptions::new().include_untracked(true)),
        )?;

        Ok(diff.deltas().len().gt(&0))
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

    fn checkout(&self, ref_name: &str) -> Result<Sha1Hash> {
        let (object, reference) = self.git_repo.revparse_ext(ref_name)?;

        self.git_repo.checkout_tree(&object, None)?;

        match reference {
            // gref is an actual reference like branches or tags
            Some(gref) => self.git_repo.set_head(gref.name().unwrap()),
            // this is a commit, not a reference
            None => self.git_repo.set_head_detached(object.id()),
        }?;
        let oid = self.git_repo.head()?.peel_to_commit()?.id();

        Ok(oid_to_sha1(&oid))
    }

    fn create_branch_at_commit(&self, branch_name: &str, commit: &str) -> Result<()> {
        let branch_checkedout = self.get_checked_out_branch_name()?.eq(branch_name);
        if branch_checkedout {
            let (name, _) = self.get_main_or_master_branch()?;
            self.checkout(name)?;
        }

        self.git_repo
            .branch(
                branch_name,
                &self.git_repo.find_commit(Oid::from_str(commit)?)?,
                true,
            )
            .context("branch could not be created")?;

        if branch_checkedout {
            self.checkout(branch_name)?;
        }
        Ok(())
    }
    /* returns patches applied */
    fn apply_patch_chain(
        &self,
        branch_name: &str,
        patch_and_ancestors: Vec<nostr::Event>,
    ) -> Result<Vec<nostr::Event>> {
        let branch_tip_result = self.get_tip_of_branch(branch_name);

        // filter out existing ancestors in branch
        let mut patches_to_apply: Vec<nostr::Event> = patch_and_ancestors
            .into_iter()
            .filter(|e| {
                let commit_id = get_commit_id_from_patch(e).unwrap();
                if let Ok(branch_tip) = branch_tip_result {
                    !branch_tip.to_string().eq(&commit_id)
                        && !self
                            .ancestor_of(&branch_tip, &str_to_sha1(&commit_id).unwrap())
                            .unwrap()
                } else {
                    true
                }
            })
            .collect();

        let parent_commit_id = tag_value(
            if let Ok(last_patch) = patches_to_apply.last().context("no patches") {
                last_patch
            } else {
                self.checkout(branch_name)
                    .context("no patches and so cannot create a proposal branch")?;
                return Ok(vec![]);
            },
            "parent-commit",
        )?;

        // check patches can be applied
        if !self.does_commit_exist(&parent_commit_id)? {
            bail!("cannot find parent commit ({parent_commit_id}). run git pull and try again.")
        }

        // checkout branch
        self.create_branch_at_commit(branch_name, &parent_commit_id)?;
        self.checkout(branch_name)?;

        // apply commits
        patches_to_apply.reverse();

        for patch in &patches_to_apply {
            let commit_id = get_commit_id_from_patch(patch)?;
            // only create new commits - otherwise make them the tip
            if self.does_commit_exist(&commit_id)? {
                self.create_branch_at_commit(branch_name, &commit_id)?;
            } else {
                apply_patch(self, patch)?;
            }
        }
        Ok(patches_to_apply)
    }

    fn parse_starting_commits(&self, starting_commits: &str) -> Result<Vec<Sha1Hash>> {
        let revspec = self
            .git_repo
            .revparse(starting_commits)
            .context("specified value not in a valid format")?;
        if revspec.mode().is_no_single() {
            let (ahead, _) = self
                .get_commits_ahead_behind(
                    &oid_to_sha1(
                        &revspec
                            .from()
                            .context("cannot get starting commit from specified value")?
                            .id(),
                    ),
                    &self
                        .get_head_commit()
                        .context("cannot get head commit with gitlib2")?,
                )
                .context("specified commit is not an ancestor of current head")?;
            Ok(ahead)
        } else if revspec.mode().is_range() {
            let (ahead, _) = self
                .get_commits_ahead_behind(
                    &oid_to_sha1(
                        &revspec
                            .from()
                            .context("cannot get starting commit of range from specified value")?
                            .id(),
                    ),
                    &oid_to_sha1(
                        &revspec
                            .to()
                            .context("cannot get end of range commit from specified value")?
                            .id(),
                    ),
                )
                .context("specified commit is not an ancestor of current head")?;
            Ok(ahead)
        } else {
            bail!("specified value not in a supported format")
        }
    }

    fn ancestor_of(&self, decendant: &Sha1Hash, ancestor: &Sha1Hash) -> Result<bool> {
        if let Ok(res) = self
            .git_repo
            .graph_descendant_of(sha1_to_oid(decendant)?, sha1_to_oid(ancestor)?)
            .context("could not run graph_descendant_of in gitlib2")
        {
            Ok(res)
        } else {
            Ok(false)
        }
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

pub fn str_to_sha1(s: &str) -> Result<Sha1Hash> {
    Ok(oid_to_sha1(
        &Oid::from_str(s).context("string is not a sha1 hash")?,
    ))
}

fn git_sig_to_tag_vec(sig: &git2::Signature) -> Vec<String> {
    vec![
        sig.name().unwrap_or("").to_string(),
        sig.email().unwrap_or("").to_string(),
        format!("{}", sig.when().seconds()),
        format!("{}", sig.when().offset_minutes()),
    ]
}

fn apply_patch(git_repo: &Repo, patch: &nostr::Event) -> Result<()> {
    // check parent commit matches head
    if !git_repo
        .get_head_commit()?
        .to_string()
        .eq(&tag_value(patch, "parent-commit")?)
    {
        bail!(
            "patch parent ({}) doesnt match current head ({})",
            tag_value(patch, "parent-commit")?,
            git_repo.get_head_commit()?
        );
    }

    let diff_from_patch = git2::Diff::from_buffer(patch.content.as_bytes()).unwrap();

    let mut apply_opts = git2::ApplyOptions::new();
    apply_opts.check(false);

    git_repo.git_repo.apply(
        &diff_from_patch,
        git2::ApplyLocation::WorkDir,
        Some(&mut apply_opts),
    )?;
    // stage and commit
    let prev_oid = git_repo.git_repo.head().unwrap().peel_to_commit()?;

    let mut index = git_repo.git_repo.index()?;
    index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;

    let pgp_sig = if let Ok(pgp_sig) = tag_value(patch, "commit-pgp-sig") {
        if pgp_sig.is_empty() {
            None
        } else {
            Some(pgp_sig)
        }
    } else {
        None
    };

    if let Some(pgp_sig) = pgp_sig {
        let commit_buff = git_repo.git_repo.commit_create_buffer(
            &extract_sig_from_patch_tags(&patch.tags, "author")?,
            &extract_sig_from_patch_tags(&patch.tags, "committer")?,
            tag_value(patch, "description")?.as_str(),
            &git_repo.git_repo.find_tree(index.write_tree()?)?,
            &[&prev_oid],
        )?;
        let gpg_commit_id = git_repo.git_repo.commit_signed(
            commit_buff.as_str().unwrap(),
            pgp_sig.as_str(),
            None,
        )?;
        git_repo.git_repo.reset(
            &git_repo.git_repo.find_object(gpg_commit_id, None)?,
            git2::ResetType::Mixed,
            None,
        )?;
        if gpg_commit_id
            .to_string()
            .eq(&get_commit_id_from_patch(patch)?)
        {
            return Ok(());
        }
    } else {
        git_repo.git_repo.commit(
            Some("HEAD"),
            &extract_sig_from_patch_tags(&patch.tags, "author")?,
            &extract_sig_from_patch_tags(&patch.tags, "committer")?,
            tag_value(patch, "description")?.as_str(),
            &git_repo.git_repo.find_tree(index.write_tree()?)?,
            &[&prev_oid],
        )?;
    }
    validate_patch_applied(git_repo, patch)
}

fn validate_patch_applied(git_repo: &Repo, patch: &nostr::Event) -> Result<()> {
    // end of stage and commit
    // check commit applied
    if git_repo
        .get_head_commit()?
        .to_string()
        .eq(&tag_value(patch, "parent-commit")?)
    {
        bail!("applying patch failed");
    }

    let mut revwalk = git_repo.git_repo.revwalk().context("revwalk error")?;
    revwalk.push_head().context("revwalk.push_head")?;

    for (i, oid) in revwalk.enumerate() {
        if i == 0 {
            let old_commit = git_repo
                .git_repo
                .find_commit(oid.context("cannot get oid in revwalk")?)
                .context("cannot find newly added commit oid")?;
            // create commit using amend which relects the original commit id
            let updated_commit_oid = old_commit
                .amend(
                    None,
                    Some(&old_commit.author()),
                    Some(&old_commit.committer()),
                    None,
                    None,
                    None,
                )
                .context("cannot amend commit to produce new oid")?;
            // replace the commit with the wrong oid with the newly created one with the
            // correct oid
            git_repo
                .git_repo
                .head()
                .context("cannot get head of git_repo")?
                .set_target(updated_commit_oid, "ref commit with fix committer details")
                .context("cannot update branch with fixed commit")?;

            if !updated_commit_oid
                .to_string()
                .eq(&get_commit_id_from_patch(patch)?)
            {
                bail!(
                    "when applied the patch commit id ({}) doesn't match the one specified in the event tag ({})",
                    updated_commit_oid.to_string(),
                    get_commit_id_from_patch(patch)?,
                )
            }
        }
    }
    Ok(())
}

fn extract_sig_from_patch_tags<'a>(
    tags: &'a [nostr::Tag],
    tag_name: &str,
) -> Result<git2::Signature<'a>> {
    let v = tags
        .iter()
        .find(|t| t.as_vec()[0].eq(tag_name))
        .context(format!("tag '{tag_name}' not present in patch"))?
        .as_vec();
    if v.len() != 5 {
        bail!("tag '{tag_name}' is incorrectly formatted")
    }
    git2::Signature::new(
        v[1].as_str(),
        v[2].as_str(),
        &git2::Time::new(
            v[3].parse().context("tag time is incorrectly formatted")?,
            v[4].parse()
                .context("tag time offset is incorrectly formatted")?,
        ),
    )
    .context("failed to create git signature")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_utils::{generate_repo_ref_event, git::GitTestRepo, TEST_KEY_1_KEYS};

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

    mod get_commit_message {
        use super::*;
        fn run(message: &str) -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            std::fs::write(test_repo.dir.join("t100.md"), "some content")?;
            let oid = test_repo.stage_and_commit(message)?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert_eq!(message, git_repo.get_commit_message(&oid_to_sha1(&oid))?,);
            Ok(())
        }
        #[test]
        fn one_liner() -> Result<()> {
            run("add t100.md")
        }

        #[test]
        fn multiline() -> Result<()> {
            run("add t100.md\r\nanother line\r\nthird line")
        }

        #[test]
        fn trailing_newlines() -> Result<()> {
            run("add t100.md\r\n\r\n\r\n\r\n\r\n\r\n")
        }

        #[test]
        fn unicode_characters() -> Result<()> {
            run("add t100.md ❤️")
        }
    }

    mod get_commit_message_summary {
        use super::*;
        fn run(message: &str, summary: &str) -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            std::fs::write(test_repo.dir.join("t100.md"), "some content")?;
            let oid = test_repo.stage_and_commit(message)?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert_eq!(
                summary,
                git_repo.get_commit_message_summary(&oid_to_sha1(&oid))?,
            );
            Ok(())
        }
        #[test]
        fn one_liner() -> Result<()> {
            run("add t100.md", "add t100.md")
        }

        #[test]
        fn multiline() -> Result<()> {
            run("add t100.md\r\nanother line\r\nthird line", "add t100.md")
        }

        #[test]
        fn trailing_newlines() -> Result<()> {
            run("add t100.md\r\n\r\n\r\n\r\n\r\n\r\n", "add t100.md")
        }

        #[test]
        fn unicode_characters() -> Result<()> {
            run("add t100.md ❤️", "add t100.md ❤️")
        }
    }

    mod get_commit_author {
        use super::*;

        static NAME: &str = "carole";
        static EMAIL: &str = "carole@pm.me";

        fn prep(time: &git2::Time) -> Result<Vec<String>> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            fs::write(test_repo.dir.join("x1.md"), "some content")?;
            let oid = test_repo.stage_and_commit_custom_signature(
                "add x1.md",
                Some(&git2::Signature::new(NAME, EMAIL, time)?),
                None,
            )?;

            let git_repo = Repo::from_path(&test_repo.dir)?;
            git_repo.get_commit_author(&oid_to_sha1(&oid))
        }

        #[test]
        fn name() -> Result<()> {
            let res = prep(&git2::Time::new(5000, 0))?;
            assert_eq!(NAME, res[0]);
            Ok(())
        }

        #[test]
        fn email() -> Result<()> {
            let res = prep(&git2::Time::new(5000, 0))?;
            assert_eq!(EMAIL, res[1]);
            Ok(())
        }

        mod time {
            use super::*;

            #[test]
            fn no_offset() -> Result<()> {
                let res = prep(&git2::Time::new(5000, 0))?;
                assert_eq!("5000", res[2]);
                assert_eq!("0", res[3]);
                Ok(())
            }
            #[test]
            fn positive_offset() -> Result<()> {
                let res = prep(&git2::Time::new(5000, 300))?;
                assert_eq!("5000", res[2]);
                assert_eq!("300", res[3]);
                Ok(())
            }
            #[test]
            fn negative_offset() -> Result<()> {
                let res = prep(&git2::Time::new(5000, -300))?;
                assert_eq!("5000", res[2]);
                assert_eq!("-300", res[3]);
                Ok(())
            }
        }

        mod extract_sig_from_patch_tags {
            use super::*;

            fn test(time: git2::Time) -> Result<()> {
                assert_eq!(
                    extract_sig_from_patch_tags(
                        &[nostr::Tag::Generic(
                            nostr::TagKind::Custom("author".to_string()),
                            prep(&time)?,
                        )],
                        "author",
                    )?
                    .to_string(),
                    git2::Signature::new(NAME, EMAIL, &time)?.to_string(),
                );
                Ok(())
            }

            #[test]
            fn no_offset() -> Result<()> {
                test(git2::Time::new(5000, 0))
            }

            #[test]
            fn positive_offset() -> Result<()> {
                test(git2::Time::new(5000, 300))
            }

            #[test]
            fn negative_offset() -> Result<()> {
                test(git2::Time::new(5000, -300))
            }
        }
    }

    mod get_commit_comitter {
        use super::*;

        static NAME: &str = "carole";
        static EMAIL: &str = "carole@pm.me";

        fn prep(time: &git2::Time) -> Result<Vec<String>> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            fs::write(test_repo.dir.join("x1.md"), "some content")?;
            let oid = test_repo.stage_and_commit_custom_signature(
                "add x1.md",
                None,
                Some(&git2::Signature::new(NAME, EMAIL, time)?),
            )?;

            let git_repo = Repo::from_path(&test_repo.dir)?;
            git_repo.get_commit_comitter(&oid_to_sha1(&oid))
        }

        #[test]
        fn name() -> Result<()> {
            let res = prep(&git2::Time::new(5000, 0))?;
            assert_eq!(NAME, res[0]);
            Ok(())
        }

        #[test]
        fn email() -> Result<()> {
            let res = prep(&git2::Time::new(5000, 0))?;
            assert_eq!(EMAIL, res[1]);
            Ok(())
        }
    }

    mod does_commit_exist {
        use super::*;

        #[test]
        fn existing_commits_results_in_true() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(git_repo.does_commit_exist("431b84edc0d2fa118d63faa3c2db9c73d630a5ae")?);
            Ok(())
        }

        #[test]
        fn correctly_formatted_hash_that_doesnt_correspond_to_an_existing_commit_results_in_false()
        -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(!git_repo.does_commit_exist("000004edc0d2fa118d63faa3c2db9c73d630a5ae")?);
            Ok(())
        }

        #[test]
        fn incorrectly_formatted_hash_that_doesnt_correspond_to_an_existing_commit_results_in_error()
        -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(git_repo.does_commit_exist("00").is_ok());
            Ok(())
        }
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
                libgit2 1.7.2\n\
                \n\
                ",
                git_repo.make_patch_from_commit(&oid_to_sha1(&oid), &None)?,
            );
            Ok(())
        }

        #[test]
        fn series_count() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let oid = test_repo.populate()?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert_eq!(
                "\
                From 431b84edc0d2fa118d63faa3c2db9c73d630a5ae Mon Sep 17 00:00:00 2001\n\
                From: Joe Bloggs <joe.bloggs@pm.me>\n\
                Date: Thu, 1 Jan 1970 00:00:00 +0000\n\
                Subject: [PATCH 3/5] add t2.md\n\
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
                libgit2 1.7.2\n\
                \n\
                ",
                git_repo.make_patch_from_commit(&oid_to_sha1(&oid), &Some((3, 5)))?,
            );
            Ok(())
        }
    }

    mod get_main_or_master_branch {

        use super::*;

        #[test]
        fn return_origin_main_if_exists() -> Result<()> {
            let test_origin_repo = GitTestRepo::new("main")?;
            let main_origin_oid = test_origin_repo.populate()?;

            let test_repo = GitTestRepo::new("main")?;
            test_repo.populate()?;
            test_repo.add_remote("origin", test_origin_repo.dir.to_str().unwrap())?;
            test_repo
                .git_repo
                .find_remote("origin")?
                .fetch(&["main"], None, None)?;

            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            test_repo.stage_and_commit("add t3.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;
            let (name, commit_hash) = git_repo.get_main_or_master_branch()?;
            assert_eq!(name, "origin/main");
            assert_eq!(commit_hash, oid_to_sha1(&main_origin_oid));
            Ok(())
        }

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

    mod get_origin_url {
        use super::*;

        #[test]
        fn returns_origin_url() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.add_remote("origin", "https://localhost:1000")?;
            let git_repo = Repo::from_path(&test_repo.dir)?;
            assert_eq!(git_repo.get_origin_url()?, "https://localhost:1000");
            Ok(())
        }
    }
    mod get_checked_out_branch_name {
        use super::*;

        #[test]
        fn returns_checked_out_branch_name() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let _ = test_repo.populate()?;
            // create feature branch
            test_repo.create_branch("example-feature")?;
            test_repo.checkout("example-feature")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert_eq!(
                git_repo.get_checked_out_branch_name()?,
                "example-feature".to_string()
            );
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

    mod create_branch_at_commit {
        use super::*;
        #[test]
        fn doesnt_error() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;
            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            let branch_name = "test-name-1";
            git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;

            Ok(())
        }

        #[test]
        fn branch_gets_created() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;
            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            let branch_name = "test-name-1";
            git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;

            assert!(test_repo.checkout(branch_name).is_ok());
            Ok(())
        }

        #[test]
        fn branch_created_with_correct_commit() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;
            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            let branch_name = "test-name-1";
            git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;

            assert_eq!(test_repo.checkout(branch_name)?, ahead_1_oid);
            Ok(())
        }

        mod when_branch_already_exists {
            use super::*;

            #[test]
            fn when_new_tip_specified_it_is_updated() -> Result<()> {
                let test_repo = GitTestRepo::default();
                test_repo.populate()?;
                // create feature branch and add 2 commits
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
                std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
                let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let branch_name = "test-name-1";
                git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;

                git_repo.create_branch_at_commit(branch_name, &ahead_2_oid.to_string())?;
                assert_eq!(test_repo.checkout(branch_name)?, ahead_2_oid);
                Ok(())
            }

            #[test]
            fn when_same_tip_is_specified_it_doesnt_error() -> Result<()> {
                let test_repo = GitTestRepo::default();
                test_repo.populate()?;
                // create feature branch and add 2 commits
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
                std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
                test_repo.stage_and_commit("add t4.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let branch_name = "test-name-1";
                git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;

                git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;
                assert_eq!(test_repo.checkout(branch_name)?, ahead_1_oid);
                Ok(())
            }

            #[test]
            fn when_branch_is_checkedout_new_tip_specified_it_is_updated() -> Result<()> {
                let test_repo = GitTestRepo::default();
                test_repo.populate()?;
                // create feature branch and add 2 commits
                test_repo.create_branch("feature")?;
                test_repo.checkout("feature")?;
                std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
                let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
                std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
                let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

                let git_repo = Repo::from_path(&test_repo.dir)?;

                let branch_name = "test-name-1";
                git_repo.create_branch_at_commit(branch_name, &ahead_1_oid.to_string())?;
                test_repo.checkout(branch_name)?;
                git_repo.create_branch_at_commit(branch_name, &ahead_2_oid.to_string())?;
                test_repo.checkout("main")?;

                assert_eq!(test_repo.checkout(branch_name)?, ahead_2_oid);
                Ok(())
            }
        }
    }

    mod apply_patch {

        use super::*;
        use crate::{repo_ref::RepoRef, sub_commands::send::generate_patch_event};

        fn generate_patch_from_head_commit(test_repo: &GitTestRepo) -> Result<nostr::Event> {
            let original_oid = test_repo.git_repo.head()?.peel_to_commit()?.id();
            let git_repo = Repo::from_path(&test_repo.dir)?;
            generate_patch_event(
                &git_repo,
                &git_repo.get_root_commit()?,
                &oid_to_sha1(&original_oid),
                Some(nostr::EventId::all_zeros()),
                &TEST_KEY_1_KEYS,
                &RepoRef::try_from(generate_repo_ref_event()).unwrap(),
                None,
                None,
                None,
                &None,
            )
        }
        fn test_patch_applies_to_repository(patch_event: nostr::Event) -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            let git_repo = Repo::from_path(&test_repo.dir)?;
            println!("{:?}", &patch_event);
            apply_patch(&git_repo, &patch_event)?;
            let commit_id = tag_value(&patch_event, "commit")?;
            // does commit with id exist?
            assert!(git_repo.does_commit_exist(&commit_id)?);
            // is commit head
            assert_eq!(
                test_repo
                    .git_repo
                    .head()?
                    .peel_to_commit()?
                    .id()
                    .to_string(),
                commit_id,
            );
            // applied to current checked branch (head hasn't moved to specific commit)
            assert_eq!(
                test_repo
                    .git_repo
                    .head()?
                    .shorthand()
                    .context("an object without a shorthand is checked out")?
                    .to_string(),
                "main",
            );

            Ok(())
        }

        mod patch_created_as_commit_with_matching_id {
            use test_utils::git::joe_signature;

            use super::*;

            #[test]
            fn simple_signature_author_committer_same_as_git_user_0_unixtime_no_pgp_signature()
            -> Result<()> {
                let source_repo = GitTestRepo::default();
                source_repo.populate()?;
                fs::write(source_repo.dir.join("x1.md"), "some content")?;
                source_repo.stage_and_commit("add x1.md")?;

                test_patch_applies_to_repository(generate_patch_from_head_commit(&source_repo)?)
            }

            #[test]
            fn signature_with_specific_author_time() -> Result<()> {
                let source_repo = GitTestRepo::default();
                source_repo.populate()?;
                fs::write(source_repo.dir.join("x1.md"), "some content")?;
                source_repo.stage_and_commit_custom_signature(
                    "add x1.md",
                    Some(&git2::Signature::new(
                        joe_signature().name().unwrap(),
                        joe_signature().email().unwrap(),
                        &git2::Time::new(5000, 0),
                    )?),
                    None,
                )?;

                test_patch_applies_to_repository(generate_patch_from_head_commit(&source_repo)?)
            }

            #[test]
            fn author_name_and_email_not_current_git_user() -> Result<()> {
                let source_repo = GitTestRepo::default();
                source_repo.populate()?;
                fs::write(source_repo.dir.join("x1.md"), "some content")?;
                source_repo.stage_and_commit_custom_signature(
                    "add x1.md",
                    Some(&git2::Signature::new(
                        "carole",
                        "carole@pm.me",
                        &git2::Time::new(0, 0),
                    )?),
                    None,
                )?;

                test_patch_applies_to_repository(generate_patch_from_head_commit(&source_repo)?)
            }

            #[test]
            fn comiiter_name_and_email_not_current_git_user_or_author() -> Result<()> {
                let source_repo = GitTestRepo::default();
                source_repo.populate()?;
                fs::write(source_repo.dir.join("x1.md"), "some content")?;
                source_repo.stage_and_commit_custom_signature(
                    "add x1.md",
                    Some(&git2::Signature::new(
                        "carole",
                        "carole@pm.me",
                        &git2::Time::new(0, 0),
                    )?),
                    Some(&git2::Signature::new(
                        "bob",
                        "bob@pm.me",
                        &git2::Time::new(0, 0),
                    )?),
                )?;

                test_patch_applies_to_repository(generate_patch_from_head_commit(&source_repo)?)
            }

            // TODO: pgp signature

            #[test]
            fn unique_author_and_commiter_details() -> Result<()> {
                let source_repo = GitTestRepo::default();
                source_repo.populate()?;
                fs::write(source_repo.dir.join("x1.md"), "some content")?;
                source_repo.stage_and_commit_custom_signature(
                    "add x1.md",
                    Some(&git2::Signature::new(
                        "carole",
                        "carole@pm.me",
                        &git2::Time::new(5000, 0),
                    )?),
                    Some(&git2::Signature::new(
                        "bob",
                        "bob@pm.me",
                        &git2::Time::new(1000, 0),
                    )?),
                )?;

                test_patch_applies_to_repository(generate_patch_from_head_commit(&source_repo)?)
            }
        }
    }

    mod apply_patch_chain {
        use test_utils::TEST_KEY_1_KEYS;

        use super::*;
        use crate::{
            repo_ref::RepoRef, sub_commands::send::generate_cover_letter_and_patch_events,
        };

        static BRANCH_NAME: &str = "add-example-feature";
        // returns original_repo, cover_letter_event, patch_events
        fn generate_test_repo_and_events() -> Result<(GitTestRepo, nostr::Event, Vec<nostr::Event>)>
        {
            let original_repo = GitTestRepo::default();
            let oid3 = original_repo.populate_with_test_branch()?;
            let oid2 = original_repo.git_repo.find_commit(oid3)?.parent_id(0)?;
            let oid1 = original_repo.git_repo.find_commit(oid2)?.parent_id(0)?;
            // TODO: generate cover_letter and patch events
            let git_repo = Repo::from_path(&original_repo.dir)?;

            let mut events = generate_cover_letter_and_patch_events(
                Some(("test".to_string(), "test".to_string())),
                &git_repo,
                &[oid_to_sha1(&oid1), oid_to_sha1(&oid2), oid_to_sha1(&oid3)],
                &TEST_KEY_1_KEYS,
                &RepoRef::try_from(generate_repo_ref_event()).unwrap(),
                &None,
            )?;

            events.reverse();

            Ok((original_repo, events.pop().unwrap(), events))
        }

        mod when_branch_and_commits_dont_exist {
            use super::*;

            mod when_branch_root_is_tip_of_main {
                use super::*;

                #[test]
                fn branch_gets_created_with_name_specified_in_proposal() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert!(
                        git_repo
                            .get_local_branch_names()?
                            .contains(&BRANCH_NAME.to_string())
                    );
                    Ok(())
                }

                #[test]
                fn branch_checked_out() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        git_repo.get_checked_out_branch_name()?,
                        BRANCH_NAME.to_string(),
                    );
                    Ok(())
                }

                #[test]
                fn patches_get_created_as_commits() -> Result<()> {
                    let (original_repo, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        test_repo.git_repo.head()?.peel_to_commit()?.id(),
                        original_repo.git_repo.head()?.peel_to_commit()?.id(),
                    );
                    Ok(())
                }

                #[test]
                fn branch_tip_is_most_recent_patch() -> Result<()> {
                    let (original_repo, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        git_repo.get_tip_of_branch(BRANCH_NAME)?,
                        oid_to_sha1(&original_repo.git_repo.head()?.peel_to_commit()?.id(),),
                    );
                    Ok(())
                }

                #[test]
                fn previously_checked_out_branch_tip_does_not_change() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let existing_branch = test_repo.get_checked_out_branch_name()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    let previous_tip_of_existing_branch =
                        git_repo.get_tip_of_branch(existing_branch.as_str())?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        previous_tip_of_existing_branch,
                        git_repo.get_tip_of_branch(existing_branch.as_str())?,
                    );
                    Ok(())
                }

                #[test]
                fn returns_all_patches_applied() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    let res = git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(res.len(), 3);
                    Ok(())
                }
            }

            mod when_branch_root_is_tip_behind_main {
                use super::*;

                #[test]
                fn branch_gets_created_with_name_specified_in_proposal() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    std::fs::write(test_repo.dir.join("m3.md"), "some content")?;
                    test_repo.stage_and_commit("add m3.md")?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert!(
                        git_repo
                            .get_local_branch_names()?
                            .contains(&BRANCH_NAME.to_string())
                    );
                    Ok(())
                }

                #[test]
                fn branch_checked_out() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    std::fs::write(test_repo.dir.join("m3.md"), "some content")?;
                    test_repo.stage_and_commit("add m3.md")?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        git_repo.get_checked_out_branch_name()?,
                        BRANCH_NAME.to_string(),
                    );
                    Ok(())
                }

                #[test]
                fn branch_tip_is_most_recent_patch() -> Result<()> {
                    let (original_repo, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    std::fs::write(test_repo.dir.join("m3.md"), "some content")?;
                    test_repo.stage_and_commit("add m3.md")?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        git_repo.get_tip_of_branch(BRANCH_NAME)?,
                        oid_to_sha1(&original_repo.git_repo.head()?.peel_to_commit()?.id(),),
                    );
                    Ok(())
                }

                #[test]
                fn previously_checked_out_branch_tip_does_not_change() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    std::fs::write(test_repo.dir.join("m3.md"), "some content")?;
                    test_repo.stage_and_commit("add m3.md")?;
                    let existing_branch = test_repo.get_checked_out_branch_name()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    let previous_tip_of_existing_branch =
                        git_repo.get_tip_of_branch(existing_branch.as_str())?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(
                        previous_tip_of_existing_branch,
                        git_repo.get_tip_of_branch(existing_branch.as_str())?,
                    );
                    Ok(())
                }

                #[test]
                fn returns_all_patches_applied() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    let res = git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(res.len(), 3);
                    Ok(())
                }
            }

            // TODO when_proposal_root_is_tip_ahead_of_main_and_doesnt_exist
        }

        mod when_branch_and_first_commits_exists {
            use super::*;

            mod when_branch_already_checked_out {
                use super::*;

                #[test]
                fn branch_tip_is_most_recent_patch() -> Result<()> {
                    let (original_repo, _, mut patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, vec![patch_events.pop().unwrap()])?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;

                    assert_eq!(
                        git_repo.get_tip_of_branch(BRANCH_NAME)?,
                        oid_to_sha1(&original_repo.git_repo.head()?.peel_to_commit()?.id(),),
                    );
                    Ok(())
                }

                #[test]
                fn returns_all_patches_applied() -> Result<()> {
                    let (_, _, mut patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, vec![patch_events.pop().unwrap()])?;
                    let res = git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(res.len(), 2);
                    Ok(())
                }
            }
            mod when_branch_not_checked_out {
                use super::*;

                #[test]
                fn branch_tip_is_most_recent_patch() -> Result<()> {
                    let (original_repo, _, mut patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, vec![patch_events.pop().unwrap()])?;
                    git_repo.checkout("main")?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;

                    assert_eq!(
                        git_repo.get_tip_of_branch(BRANCH_NAME)?,
                        oid_to_sha1(&original_repo.git_repo.head()?.peel_to_commit()?.id(),),
                    );
                    Ok(())
                }

                #[test]
                fn branch_checked_out() -> Result<()> {
                    let (_, _, mut patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, vec![patch_events.pop().unwrap()])?;
                    git_repo.checkout("main")?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;

                    assert_eq!(
                        git_repo.get_checked_out_branch_name()?,
                        BRANCH_NAME.to_string(),
                    );
                    Ok(())
                }

                #[test]
                fn returns_all_patches_applied() -> Result<()> {
                    let (_, _, mut patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, vec![patch_events.pop().unwrap()])?;
                    git_repo.checkout("main")?;
                    let res = git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(res.len(), 2);
                    Ok(())
                }
            }
            // TODO when branch ahead (rebased or user commits)
        }
        mod when_branch_exists_and_is_up_to_date {
            use super::*;

            mod when_branch_already_checked_out {
                use super::*;

                #[test]
                fn returns_all_patches_applied_0() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events.clone())?;
                    let res = git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(res.len(), 0);
                    Ok(())
                }
            }
            mod when_branch_not_checked_out {
                use super::*;

                #[test]
                fn branch_checked_out() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events.clone())?;
                    git_repo.checkout("main")?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;

                    assert_eq!(
                        git_repo.get_checked_out_branch_name()?,
                        BRANCH_NAME.to_string(),
                    );
                    Ok(())
                }

                #[test]
                fn returns_all_patches_applied_0() -> Result<()> {
                    let (_, _, patch_events) = generate_test_repo_and_events()?;
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    let git_repo = Repo::from_path(&test_repo.dir)?;
                    git_repo.apply_patch_chain(BRANCH_NAME, patch_events.clone())?;
                    git_repo.checkout("main")?;
                    let res = git_repo.apply_patch_chain(BRANCH_NAME, patch_events)?;
                    assert_eq!(res.len(), 0);
                    Ok(())
                }
            }
        }
    }
    mod parse_starting_commits {
        use super::*;

        mod head_1_returns_latest_commit {
            use super::*;

            #[test]
            fn when_on_main_and_other_commits_are_more_recent_on_feature_branch() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let git_repo = Repo::from_path(&test_repo.dir)?;
                test_repo.populate_with_test_branch()?;
                test_repo.checkout("main")?;

                assert_eq!(
                    git_repo.parse_starting_commits("HEAD~1")?,
                    vec![str_to_sha1("431b84edc0d2fa118d63faa3c2db9c73d630a5ae")?],
                );
                Ok(())
            }

            #[test]
            fn when_checked_out_branch_ahead_of_main() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let git_repo = Repo::from_path(&test_repo.dir)?;
                test_repo.populate_with_test_branch()?;

                assert_eq!(
                    git_repo.parse_starting_commits("HEAD~1")?,
                    vec![str_to_sha1("82ff2bcc9aa94d1bd8faee723d4c8cc190d6061c")?],
                );
                Ok(())
            }
        }
        mod head_2_returns_latest_2_commits_youngest_first {
            use super::*;

            #[test]
            fn when_on_main_and_other_commits_are_more_recent_on_feature_branch() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let git_repo = Repo::from_path(&test_repo.dir)?;
                test_repo.populate_with_test_branch()?;
                test_repo.checkout("main")?;

                assert_eq!(
                    git_repo.parse_starting_commits("HEAD~2")?,
                    vec![
                        str_to_sha1("431b84edc0d2fa118d63faa3c2db9c73d630a5ae")?,
                        str_to_sha1("af474d8d271490e5c635aad337abdc050034b16a")?,
                    ],
                );
                Ok(())
            }
        }
        mod head_3_returns_latest_3_commits_youngest_first {
            use super::*;

            #[test]
            fn when_checked_out_branch_ahead_of_main() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let git_repo = Repo::from_path(&test_repo.dir)?;
                test_repo.populate_with_test_branch()?;

                assert_eq!(
                    git_repo.parse_starting_commits("HEAD~3")?,
                    vec![
                        str_to_sha1("82ff2bcc9aa94d1bd8faee723d4c8cc190d6061c")?,
                        str_to_sha1("a23e6b05aaeb7d1471b4a838b51f337d5644eeb0")?,
                        str_to_sha1("7ab82116068982671a8111f27dc10599172334b2")?,
                    ],
                );
                Ok(())
            }
        }
        mod range_of_3_commits_not_in_branch_history_returns_3_commits_youngest_first {
            use super::*;

            #[test]
            fn when_checked_out_branch_ahead_of_main() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let git_repo = Repo::from_path(&test_repo.dir)?;
                test_repo.populate_with_test_branch()?;
                test_repo.checkout("main")?;

                assert_eq!(
                    git_repo.parse_starting_commits("af474d8..a23e6b0")?,
                    vec![
                        str_to_sha1("a23e6b05aaeb7d1471b4a838b51f337d5644eeb0")?,
                        str_to_sha1("7ab82116068982671a8111f27dc10599172334b2")?,
                        str_to_sha1("431b84edc0d2fa118d63faa3c2db9c73d630a5ae")?,
                    ],
                );
                Ok(())
            }
        }
    }
    mod ancestor_of {
        use super::*;

        #[test]
        fn deep_ancestor_returns_true() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let from_main_in_feature_history = test_repo.populate()?;

            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;

            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(git_repo.ancestor_of(
                &oid_to_sha1(&ahead_2_oid),
                &oid_to_sha1(&from_main_in_feature_history)
            )?);
            Ok(())
        }

        #[test]
        fn commit_parent_returns_true() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;

            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;

            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let ahead_1_oid = test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(git_repo.ancestor_of(&oid_to_sha1(&ahead_2_oid), &oid_to_sha1(&ahead_1_oid))?);
            Ok(())
        }

        #[test]
        fn same_commit_returns_false() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;

            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;

            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(!git_repo.ancestor_of(&oid_to_sha1(&ahead_2_oid), &oid_to_sha1(&ahead_2_oid))?);
            Ok(())
        }

        #[test]
        fn commit_not_in_history_returns_false() -> Result<()> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;

            // create feature branch and add 2 commits
            test_repo.create_branch("feature")?;

            // create commit not in feature history
            std::fs::write(test_repo.dir.join("notfeature.md"), "some content")?;
            let on_main_after_feature = test_repo.stage_and_commit("add notfeature.md")?;

            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            test_repo.stage_and_commit("add t3.md")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            let ahead_2_oid = test_repo.stage_and_commit("add t4.md")?;

            let git_repo = Repo::from_path(&test_repo.dir)?;

            assert!(!git_repo.ancestor_of(
                &oid_to_sha1(&ahead_2_oid),
                &oid_to_sha1(&on_main_after_feature)
            )?);
            Ok(())
        }
    }
}
