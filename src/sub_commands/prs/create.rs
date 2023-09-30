use anyhow::{bail, Context, Result};
use nostr::{prelude::sha1::Hash as Sha1Hash, EventBuilder, Marker, Tag, TagKind};

use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms},
    git::{Repo, RepoActions},
    login, Cli,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[clap(short, long)]
    /// title of pull request (defaults to first line of first commit)
    title: Option<String>,
    #[clap(short, long)]
    /// optional description
    description: Option<String>,
    #[clap(long)]
    /// branch to get changes from (defaults to head)
    from_branch: Option<String>,
    #[clap(long)]
    /// destination branch (defaults to main or master)
    to_branch: Option<String>,
}

pub fn launch(
    cli_args: &Cli,
    _pr_args: &super::SubCommandArgs,
    args: &SubCommandArgs,
) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let (from_branch, to_branch, ahead, behind) =
        identify_ahead_behind(&git_repo, &args.from_branch, &args.to_branch)?;

    if ahead.is_empty() {
        bail!(format!(
            "'{from_branch}' is 0 commits ahead of '{to_branch}' so no patches were created"
        ));
    }

    if behind.is_empty() {
        println!(
            "creating patch for {} commits from '{from_branch}' that can be merged into '{to_branch}'",
            ahead.len(),
        );
    } else {
        if !Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt(
                    format!(
                        "'{from_branch}' is {} commits behind '{to_branch}' and {} ahead. Consider rebasing before sending patches. Proceed anyway?",
                        behind.len(),
                        ahead.len(),
                    )
                )
                .with_default(false)
        ).context("failed to get confirmation response from interactor confirm")? {
            bail!("aborting so branch can be rebased");
        }
        println!(
            "creating patch for {} commit{} from '{from_branch}' that {} {} behind '{to_branch}'",
            ahead.len(),
            if ahead.len() > 1 { "s" } else { "" },
            if ahead.len() > 1 { "are" } else { "is" },
            behind.len(),
        );
    }

    let title = match &args.title {
        Some(t) => t.clone(),
        None => Interactor::default().input(PromptInputParms::default().with_prompt("title"))?,
    };

    let description = match &args.description {
        Some(t) => t.clone(),
        None => Interactor::default()
            .input(PromptInputParms::default().with_prompt("description (Optional)"))?,
    };

    let root_commit = git_repo
        .get_root_commit(to_branch.as_str())
        .context("failed to get root commit of the repository")?;
    // create PR event

    let keys = login::launch(&cli_args.nsec, &cli_args.password)?;

    let pr_event = EventBuilder::new(
        nostr::event::Kind::Custom(318),
        format!("{title}\r\n\r\n{description}"),
        &[Tag::Hashtag(format!("r-{root_commit}"))],
        // TODO: suggested branch name
        // Tag::Generic(
        //     TagKind::Custom("suggested-branch-name".to_string()),
        //     vec![],
        // ),
        // TODO: add Repo event as root
        // TODO: people tag maintainers
        // TODO: add relay tags
    )
    .to_event(&keys)
    .context("failed to create pr event")?;

    let mut patch_events = vec![];
    for commit in &ahead {
        let commit_parent = git_repo
            .get_commit_parent(commit)
            .context("failed to create patch event")?;
        patch_events.push(
            EventBuilder::new(
                nostr::event::Kind::Custom(317),
                git_repo
                    .make_patch_from_commit(commit)
                    .context(format!("cannot make patch for commit {commit}"))?,
                &[
                    Tag::Hashtag(format!("r-{root_commit}")),
                    Tag::Hashtag(commit.to_string()),
                    Tag::Hashtag(commit_parent.to_string()),
                    Tag::Event(
                        pr_event.id,
                        None, // TODO: add relay
                        Some(Marker::Root),
                    ),
                    Tag::Generic(
                        TagKind::Custom("commit".to_string()),
                        vec![commit.to_string()],
                    ),
                    Tag::Generic(
                        TagKind::Custom("parent-commit".to_string()),
                        vec![commit_parent.to_string()],
                    ),
                    // TODO: add Repo event tags
                    // TODO: people tag maintainers
                    // TODO: add relay tags
                ],
            )
            .to_event(&keys),
        );
    }

    // TODO check if there is already a similarly named PR
    // TODO connect to relays and post

    Ok(())
}

// TODO
// - find profile
// - file relays
// - find repo events
// -

/**
 * returns `(from_branch,to_branch,ahead,behind)`
 */
fn identify_ahead_behind(
    git_repo: &Repo,
    from_branch: &Option<String>,
    to_branch: &Option<String>,
) -> Result<(String, String, Vec<Sha1Hash>, Vec<Sha1Hash>)> {
    let (from_branch, from_tip) = match from_branch {
        Some(name) => (
            name.to_string(),
            git_repo
                .get_tip_of_local_branch(name)
                .context(format!("cannot find from_branch '{name}'"))?,
        ),
        None => (
            "head".to_string(),
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
                .get_tip_of_local_branch(name)
                .context(format!("cannot find to_branch '{name}'"))?,
        ),
        None => {
            let (name, commit) = git_repo
                .get_main_or_master_branch()
                .context("a destination branch (to_branch) is not specified and the defaults (main or master) do not exist")?;
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
    mod identify_ahead_behind {

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
                "a destination branch (to_branch) is not specified and the defaults (main or master) do not exist",
            );
            Ok(())
        }

        #[test]
        fn when_from_branch_is_none_return_as_head() -> Result<()> {
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
            // checkout feature
            test_repo.checkout("feature")?;

            let (from_branch, to_branch, ahead, behind) =
                identify_ahead_behind(&git_repo, &None, &None)?;

            assert_eq!(from_branch, "head");
            assert_eq!(ahead, vec![oid_to_sha1(&head_oid)]);
            assert_eq!(to_branch, "main");
            assert_eq!(behind, vec![oid_to_sha1(&main_oid)]);
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
}
