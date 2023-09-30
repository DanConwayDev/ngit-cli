use anyhow::Result;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

#[test]
fn when_to_branch_doesnt_exist_return_error() -> Result<()> {
    let test_repo = GitTestRepo::default();
    test_repo.populate()?;
    let mut p = CliTester::new_from_dir(
        &test_repo.dir,
        ["prs", "create", "--to-branch", "nonexistant"],
    );
    p.expect("Error: cannot find to_branch 'nonexistant'")?;
    Ok(())
}

#[test]
fn when_no_to_branch_specified_and_no_main_or_master_branch_return_error() -> Result<()> {
    let test_repo = GitTestRepo::new("notmain")?;
    test_repo.populate()?;
    let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "create"]);
    p.expect("Error: a destination branch (to_branch) is not specified and the defaults (main or master) do not exist")?;
    Ok(())
}

#[test]
fn when_from_branch_doesnt_exist_return_error() -> Result<()> {
    let test_repo = GitTestRepo::default();
    test_repo.populate()?;
    let mut p = CliTester::new_from_dir(
        &test_repo.dir,
        ["prs", "create", "--from-branch", "nonexistant"],
    );
    p.expect("Error: cannot find from_branch 'nonexistant'")?;
    Ok(())
}

#[test]
fn when_no_commits_ahead_of_main_return_error() -> Result<()> {
    let test_repo = GitTestRepo::default();
    test_repo.populate()?;
    // create feature branch with 1 commit ahead
    test_repo.create_branch("feature")?;
    test_repo.checkout("feature")?;

    let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "create"]);
    p.expect("Error: 'head' is 0 commits ahead of 'main' so no patches were created")?;
    Ok(())
}

mod when_commits_behind_ask_to_proceed {
    use super::*;

    fn prep_test_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::default();
        test_repo.populate()?;
        // create feature branch with 2 commit ahead
        test_repo.create_branch("feature")?;
        test_repo.checkout("feature")?;
        std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
        test_repo.stage_and_commit("add t3.md")?;
        std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
        test_repo.stage_and_commit("add t4.md")?;
        // checkout main and add 1 commit
        test_repo.checkout("main")?;
        std::fs::write(test_repo.dir.join("t5.md"), "some content")?;
        test_repo.stage_and_commit("add t5.md")?;
        // checkout feature branch
        test_repo.checkout("feature")?;
        Ok(test_repo)
    }
    static BEHIND_LEN: u8 = 1;
    static AHEAD_LEN: u8 = 2;

    fn expect_confirm_prompt(
        p: &mut CliTester,
        behind: u8,
        ahead: u8,
    ) -> Result<CliTesterConfirmPrompt> {
        p.expect_confirm(
            format!("'head' is {behind} commits behind 'main' and {ahead} ahead. Consider rebasing before sending patches. Proceed anyway?").as_str(),
            Some(false),
        )
    }

    #[test]
    fn asked_with_default_no() -> Result<()> {
        let test_repo = prep_test_repo()?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "create"]);
        expect_confirm_prompt(&mut p, BEHIND_LEN, AHEAD_LEN)?;
        p.exit()?;
        Ok(())
    }

    #[test]
    fn when_response_is_false_aborts() -> Result<()> {
        let test_repo = prep_test_repo()?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "create"]);

        expect_confirm_prompt(&mut p, BEHIND_LEN, AHEAD_LEN)?.succeeds_with(Some(false))?;

        p.expect_end_with("Error: aborting so branch can be rebased\r\n")?;

        Ok(())
    }
    #[test]
    #[serial]
    fn when_response_is_true_proceeds() -> Result<()> {
        let test_repo = prep_test_repo()?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "create"]);
        expect_confirm_prompt(&mut p, BEHIND_LEN, AHEAD_LEN)?.succeeds_with(Some(true))?;
        p.expect(
            format!("creating patch for {AHEAD_LEN} commits from 'head' that are {BEHIND_LEN} behind 'main'",)
                .as_str(),
        )?;
        p.exit()?;
        Ok(())
    }
}

mod when_no_commits_behind {
    use super::*;

    #[test]
    #[serial]
    fn message_for_creating_patches() -> Result<()> {
        let test_repo = GitTestRepo::default();
        test_repo.populate()?;
        // create feature branch with 2 commit ahead
        test_repo.create_branch("feature")?;
        test_repo.checkout("feature")?;
        std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
        test_repo.stage_and_commit("add t3.md")?;
        std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
        test_repo.stage_and_commit("add t4.md")?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "create"]);

        p.expect("creating patch for 2 commits from 'head' that can be merged into 'main'")?;
        p.exit()?;
        Ok(())
    }
}

// #[test]
// #[serial]
// fn succeeds_with_text_logged_in_as_npub() -> Result<()> {
//     with_fresh_config(|| {
//         let mut p = CliTester::new(["login"]);

//         p.expect_input(EXPECTED_NSEC_PROMPT)?
//             .succeeds_with(TEST_KEY_1_NSEC)?;

//         p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
//             .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
//             .succeeds_with(TEST_PASSWORD)?;

//         p.expect_end_with(format!("logged in as {}\r\n",
// TEST_KEY_1_NPUB).as_str())     })
// }
