use anyhow::{Context, Result};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;

use super::{Repo, RepoActions};

/**
 * returns `(from_branch,to_branch,ahead,behind)`
 */
pub fn identify_ahead_behind(
    git_repo: &Repo,
    from_branch: &Option<String>,
    to_branch: &Option<String>,
) -> Result<(String, String, Vec<Sha1Hash>, Vec<Sha1Hash>)> {
    let (from_branch, from_tip) = match from_branch {
        Some(name) => (
            name.to_string(),
            git_repo
                .get_tip_of_branch(name)
                .context(format!("cannot find from_branch '{name}'"))?,
        ),
        None => (
            if let Ok(name) = git_repo.get_checked_out_branch_name() {
                name
            } else {
                "head".to_string()
            },
            git_repo
                .get_head_commit()
                .context("failed to get head commit")
                .context(
                    "checkout a commit or specify a from_branch. head does not reveal a commit",
                )?,
        ),
    };

    let (to_branch, to_tip) = match to_branch {
        Some(name) => (
            name.to_string(),
            git_repo
                .get_tip_of_branch(name)
                .context(format!("cannot find to_branch '{name}'"))?,
        ),
        None => {
            let (name, commit) = git_repo
                .get_main_or_master_branch()
                .context("the default branches (main or master) do not exist")?;
            (name.to_string(), commit)
        }
    };

    match git_repo.get_commits_ahead_behind(&to_tip, &from_tip) {
        Err(e) => {
            if e.to_string().contains("is not an ancestor of") {
                return Err(e).context(format!(
                    "'{from_branch}' is not branched from '{to_branch}'"
                ));
            }
            Err(e).context(format!(
                "failed to get commits ahead and behind from '{from_branch}' to '{to_branch}'"
            ))
        }
        Ok((ahead, behind)) => Ok((from_branch, to_branch, ahead, behind)),
    }
}

#[cfg(test)]
mod tests {

    use test_utils::git::GitTestRepo;

    use super::*;
    use crate::git::oid_to_sha1;

    #[test]
    fn when_from_branch_doesnt_exist_return_error() -> Result<()> {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir)?;

        test_repo.populate()?;
        let branch_name = "doesnt_exist";
        assert_eq!(
            identify_ahead_behind(&git_repo, &Some(branch_name.to_string()), &None)
                .unwrap_err()
                .to_string(),
            format!("cannot find from_branch '{}'", &branch_name),
        );
        Ok(())
    }

    #[test]
    fn when_to_branch_doesnt_exist_return_error() -> Result<()> {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir)?;

        test_repo.populate()?;
        let branch_name = "doesnt_exist";
        assert_eq!(
            identify_ahead_behind(&git_repo, &None, &Some(branch_name.to_string()))
                .unwrap_err()
                .to_string(),
            format!("cannot find to_branch '{}'", &branch_name),
        );
        Ok(())
    }

    #[test]
    fn when_to_branch_is_none_and_no_main_or_master_branch_return_error() -> Result<()> {
        let test_repo = GitTestRepo::new("notmain")?;
        let git_repo = Repo::from_path(&test_repo.dir)?;

        test_repo.populate()?;

        assert_eq!(
            identify_ahead_behind(&git_repo, &None, &None)
                .unwrap_err()
                .to_string(),
            "the default branches (main or master) do not exist",
        );
        Ok(())
    }

    #[test]
    fn when_from_branch_is_not_head_return_as_from_branch() -> Result<()> {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir)?;

        test_repo.populate()?;
        // create feature branch with 1 commit ahead
        test_repo.create_branch("feature")?;
        test_repo.checkout("feature")?;
        std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
        let head_oid = test_repo.stage_and_commit("add t3.md")?;

        // make feature branch 1 commit behind
        test_repo.checkout("main")?;
        std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
        let main_oid = test_repo.stage_and_commit("add t4.md")?;

        let (from_branch, to_branch, ahead, behind) =
            identify_ahead_behind(&git_repo, &Some("feature".to_string()), &None)?;

        assert_eq!(from_branch, "feature");
        assert_eq!(ahead, vec![oid_to_sha1(&head_oid)]);
        assert_eq!(to_branch, "main");
        assert_eq!(behind, vec![oid_to_sha1(&main_oid)]);
        Ok(())
    }

    #[test]
    fn when_to_branch_is_not_main_return_as_to_branch() -> Result<()> {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir)?;

        test_repo.populate()?;
        // create dev branch with 1 commit ahead
        test_repo.create_branch("dev")?;
        test_repo.checkout("dev")?;
        std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
        let dev_oid_first = test_repo.stage_and_commit("add t3.md")?;

        // create feature branch with 1 commit ahead of dev
        test_repo.create_branch("feature")?;
        test_repo.checkout("feature")?;
        std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
        let feature_oid = test_repo.stage_and_commit("add t4.md")?;

        // make feature branch 1 behind
        test_repo.checkout("dev")?;
        std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
        let dev_oid = test_repo.stage_and_commit("add t3.md")?;

        let (from_branch, to_branch, ahead, behind) = identify_ahead_behind(
            &git_repo,
            &Some("feature".to_string()),
            &Some("dev".to_string()),
        )?;

        assert_eq!(from_branch, "feature");
        assert_eq!(ahead, vec![oid_to_sha1(&feature_oid)]);
        assert_eq!(to_branch, "dev");
        assert_eq!(behind, vec![oid_to_sha1(&dev_oid)]);

        let (from_branch, to_branch, ahead, behind) =
            identify_ahead_behind(&git_repo, &Some("feature".to_string()), &None)?;

        assert_eq!(from_branch, "feature");
        assert_eq!(
            ahead,
            vec![oid_to_sha1(&feature_oid), oid_to_sha1(&dev_oid_first)]
        );
        assert_eq!(to_branch, "main");
        assert_eq!(behind, vec![]);

        Ok(())
    }
}
