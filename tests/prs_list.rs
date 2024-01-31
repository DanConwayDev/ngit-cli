use anyhow::Result;
use futures::join;
use serial_test::serial;
use test_utils::{git::GitTestRepo, relay::Relay, *};

static FEATURE_BRANCH_NAME_1: &str = "feature-example-t";
static FEATURE_BRANCH_NAME_2: &str = "feature-example-f";
static FEATURE_BRANCH_NAME_3: &str = "feature-example-c";

static PR_TITLE_1: &str = "pr a";
static PR_TITLE_2: &str = "pr b";
static PR_TITLE_3: &str = "pr c";

fn cli_tester_create_prs() -> Result<GitTestRepo> {
    let git_repo = GitTestRepo::default();
    git_repo.populate()?;
    cli_tester_create_pr(
        &git_repo,
        FEATURE_BRANCH_NAME_1,
        "a",
        PR_TITLE_1,
        "pr a description",
    )?;
    cli_tester_create_pr(
        &git_repo,
        FEATURE_BRANCH_NAME_2,
        "b",
        PR_TITLE_2,
        "pr b description",
    )?;
    cli_tester_create_pr(
        &git_repo,
        FEATURE_BRANCH_NAME_3,
        "c",
        PR_TITLE_3,
        "pr c description",
    )?;
    Ok(git_repo)
}

fn create_and_populate_branch(
    test_repo: &GitTestRepo,
    branch_name: &str,
    prefix: &str,
    only_one_commit: bool,
) -> Result<()> {
    test_repo.checkout("main")?;
    test_repo.create_branch(branch_name)?;
    test_repo.checkout(branch_name)?;
    std::fs::write(
        test_repo.dir.join(format!("{}3.md", prefix)),
        "some content",
    )?;
    test_repo.stage_and_commit(format!("add {}3.md", prefix).as_str())?;
    if !only_one_commit {
        std::fs::write(
            test_repo.dir.join(format!("{}4.md", prefix)),
            "some content",
        )?;
        test_repo.stage_and_commit(format!("add {}4.md", prefix).as_str())?;
    }
    Ok(())
}

fn cli_tester_create_pr(
    test_repo: &GitTestRepo,
    branch_name: &str,
    prefix: &str,
    title: &str,
    description: &str,
) -> Result<()> {
    create_and_populate_branch(test_repo, branch_name, prefix, false)?;

    let mut p = CliTester::new_from_dir(
        &test_repo.dir,
        [
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "prs",
            "create",
            "--title",
            format!("\"{title}\"").as_str(),
            "--description",
            format!("\"{description}\"").as_str(),
        ],
    );
    p.expect_end_eventually()?;
    Ok(())
}

mod when_main_branch_is_uptodate {
    use super::*;

    mod when_pr_branch_doesnt_exist {
        use super::*;

        mod when_main_is_checked_out {
            use super::*;

            mod when_first_pr_selected {
                use super::*;

                // TODO: test when other prs with the same name but from other repositories are
                //       present on relays
                async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
                    // fallback (51,52) user write (53, 55) repo (55, 56)
                    let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                        Relay::new(8051, None, None),
                        Relay::new(8052, None, None),
                        Relay::new(8053, None, None),
                        Relay::new(8055, None, None),
                        Relay::new(8056, None, None),
                    );

                    r51.events.push(generate_test_key_1_relay_list_event());
                    r51.events.push(generate_test_key_1_metadata_event("fred"));
                    r51.events.push(generate_repo_ref_event());

                    r55.events.push(generate_repo_ref_event());
                    r55.events.push(generate_test_key_1_metadata_event("fred"));
                    r55.events.push(generate_test_key_1_relay_list_event());

                    let cli_tester_handle =
                        std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                            let originating_repo = cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(0, true)?;
                            let mut confirm =
                                p.expect_confirm_eventually("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect_end_eventually_and_print()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok((originating_repo, test_repo))
                        });

                    // launch relay
                    let _ = join!(
                        r51.listen_until_close(),
                        r52.listen_until_close(),
                        r53.listen_until_close(),
                        r55.listen_until_close(),
                        r56.listen_until_close(),
                    );
                    let res = cli_tester_handle.join().unwrap()?;

                    Ok(res)
                }

                mod cli_prompts {
                    use super::*;
                    async fn run_async_prompts_to_choose_from_pr_titles() -> Result<()> {
                        let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                            Relay::new(8051, None, None),
                            Relay::new(8052, None, None),
                            Relay::new(8053, None, None),
                            Relay::new(8055, None, None),
                            Relay::new(8056, None, None),
                        );

                        r51.events.push(generate_test_key_1_relay_list_event());
                        r51.events.push(generate_test_key_1_metadata_event("fred"));
                        r51.events.push(generate_repo_ref_event());

                        r55.events.push(generate_repo_ref_event());
                        r55.events.push(generate_test_key_1_metadata_event("fred"));
                        r55.events.push(generate_test_key_1_relay_list_event());

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(0, true)?;
                            p.expect("finding commits...\r\n")?;
                            let mut confirm = p.expect_confirm("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect("checked out PR branch. pulled 2 new commits\r\n")?;
                            p.expect_end()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        });

                        // launch relay
                        let _ = join!(
                            r51.listen_until_close(),
                            r52.listen_until_close(),
                            r53.listen_until_close(),
                            r55.listen_until_close(),
                            r56.listen_until_close(),
                        );
                        cli_tester_handle.join().unwrap()?;
                        println!("{:?}", r55.events);
                        Ok(())
                    }

                    #[tokio::test]
                    #[serial]
                    async fn prompts_to_choose_from_pr_titles() -> Result<()> {
                        let _ = run_async_prompts_to_choose_from_pr_titles().await;
                        Ok(())
                    }
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_created_with_correct_name() -> Result<()> {
                    let (_, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        vec![FEATURE_BRANCH_NAME_1, "main"],
                        test_repo.get_local_branch_names()?
                    );
                    Ok(())
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_checked_out() -> Result<()> {
                    let (_, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        FEATURE_BRANCH_NAME_1,
                        test_repo.get_checked_out_branch_name()?,
                    );
                    Ok(())
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_tip_is_most_recent_patch() -> Result<()> {
                    let (originating_repo, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                        test_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                    );
                    Ok(())
                }
            }
            mod when_third_pr_selected {
                use super::*;

                async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
                    // fallback (51,52) user write (53, 55) repo (55, 56)
                    let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                        Relay::new(8051, None, None),
                        Relay::new(8052, None, None),
                        Relay::new(8053, None, None),
                        Relay::new(8055, None, None),
                        Relay::new(8056, None, None),
                    );

                    r51.events.push(generate_test_key_1_relay_list_event());
                    r51.events.push(generate_test_key_1_metadata_event("fred"));
                    r51.events.push(generate_repo_ref_event());

                    r55.events.push(generate_repo_ref_event());
                    r55.events.push(generate_test_key_1_metadata_event("fred"));
                    r55.events.push(generate_test_key_1_relay_list_event());

                    let cli_tester_handle =
                        std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                            let originating_repo = cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(2, true)?;
                            let mut confirm =
                                p.expect_confirm_eventually("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect_end_eventually_and_print()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok((originating_repo, test_repo))
                        });

                    // launch relay
                    let _ = join!(
                        r51.listen_until_close(),
                        r52.listen_until_close(),
                        r53.listen_until_close(),
                        r55.listen_until_close(),
                        r56.listen_until_close(),
                    );
                    let res = cli_tester_handle.join().unwrap()?;

                    Ok(res)
                }

                mod cli_prompts {
                    use super::*;
                    async fn run_async_prompts_to_choose_from_pr_titles() -> Result<()> {
                        let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                            Relay::new(8051, None, None),
                            Relay::new(8052, None, None),
                            Relay::new(8053, None, None),
                            Relay::new(8055, None, None),
                            Relay::new(8056, None, None),
                        );

                        r51.events.push(generate_test_key_1_relay_list_event());
                        r51.events.push(generate_test_key_1_metadata_event("fred"));
                        r51.events.push(generate_repo_ref_event());

                        r55.events.push(generate_repo_ref_event());
                        r55.events.push(generate_test_key_1_metadata_event("fred"));
                        r55.events.push(generate_test_key_1_relay_list_event());

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(2, true)?;
                            p.expect("finding commits...\r\n")?;
                            let mut confirm = p.expect_confirm("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect("checked out PR branch. pulled 2 new commits\r\n")?;
                            p.expect_end()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        });

                        // launch relay
                        let _ = join!(
                            r51.listen_until_close(),
                            r52.listen_until_close(),
                            r53.listen_until_close(),
                            r55.listen_until_close(),
                            r56.listen_until_close(),
                        );
                        cli_tester_handle.join().unwrap()?;
                        println!("{:?}", r55.events);
                        Ok(())
                    }

                    #[tokio::test]
                    #[serial]
                    async fn prompts_to_choose_from_pr_titles() -> Result<()> {
                        let _ = run_async_prompts_to_choose_from_pr_titles().await;
                        Ok(())
                    }
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_created_with_correct_name() -> Result<()> {
                    let (_, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        vec![FEATURE_BRANCH_NAME_3, "main"],
                        test_repo.get_local_branch_names()?
                    );
                    Ok(())
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_checked_out() -> Result<()> {
                    let (_, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        FEATURE_BRANCH_NAME_3,
                        test_repo.get_checked_out_branch_name()?,
                    );
                    Ok(())
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_tip_is_most_recent_patch() -> Result<()> {
                    let (originating_repo, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_3)?,
                        test_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_3)?,
                    );
                    Ok(())
                }
            }
        }
    }

    mod when_pr_branch_exists {
        use super::*;

        mod when_main_is_checked_out {
            use super::*;

            mod when_branch_is_up_to_date {
                use super::*;
                async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
                    // fallback (51,52) user write (53, 55) repo (55, 56)
                    let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                        Relay::new(8051, None, None),
                        Relay::new(8052, None, None),
                        Relay::new(8053, None, None),
                        Relay::new(8055, None, None),
                        Relay::new(8056, None, None),
                    );

                    r51.events.push(generate_test_key_1_relay_list_event());
                    r51.events.push(generate_test_key_1_metadata_event("fred"));
                    r51.events.push(generate_repo_ref_event());

                    r55.events.push(generate_repo_ref_event());
                    r55.events.push(generate_test_key_1_metadata_event("fred"));
                    r55.events.push(generate_test_key_1_relay_list_event());

                    let cli_tester_handle =
                        std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                            let originating_repo = cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            create_and_populate_branch(
                                &test_repo,
                                FEATURE_BRANCH_NAME_1,
                                "a",
                                false,
                            )?;
                            test_repo.checkout("main")?;
                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(0, true)?;
                            let mut confirm =
                                p.expect_confirm_eventually("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect_end_eventually_and_print()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok((originating_repo, test_repo))
                        });

                    // launch relay
                    let _ = join!(
                        r51.listen_until_close(),
                        r52.listen_until_close(),
                        r53.listen_until_close(),
                        r55.listen_until_close(),
                        r56.listen_until_close(),
                    );
                    let res = cli_tester_handle.join().unwrap()?;

                    Ok(res)
                }

                mod cli_prompts {
                    use super::*;
                    async fn run_async_prompts_to_choose_from_pr_titles() -> Result<()> {
                        let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                            Relay::new(8051, None, None),
                            Relay::new(8052, None, None),
                            Relay::new(8053, None, None),
                            Relay::new(8055, None, None),
                            Relay::new(8056, None, None),
                        );

                        r51.events.push(generate_test_key_1_relay_list_event());
                        r51.events.push(generate_test_key_1_metadata_event("fred"));
                        r51.events.push(generate_repo_ref_event());

                        r55.events.push(generate_repo_ref_event());
                        r55.events.push(generate_test_key_1_metadata_event("fred"));
                        r55.events.push(generate_test_key_1_relay_list_event());

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            create_and_populate_branch(
                                &test_repo,
                                FEATURE_BRANCH_NAME_1,
                                "a",
                                false,
                            )?;
                            test_repo.checkout("main")?;

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(0, true)?;
                            p.expect("finding commits...\r\n")?;
                            let mut confirm = p.expect_confirm("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect("checked out PR branch. no new commits to pull\r\n")?;
                            p.expect_end()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        });

                        // launch relay
                        let _ = join!(
                            r51.listen_until_close(),
                            r52.listen_until_close(),
                            r53.listen_until_close(),
                            r55.listen_until_close(),
                            r56.listen_until_close(),
                        );
                        cli_tester_handle.join().unwrap()?;
                        println!("{:?}", r55.events);
                        Ok(())
                    }

                    #[tokio::test]
                    #[serial]
                    async fn prompts_to_choose_from_pr_titles() -> Result<()> {
                        let _ = run_async_prompts_to_choose_from_pr_titles().await;
                        Ok(())
                    }
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_checked_out() -> Result<()> {
                    let (_, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        FEATURE_BRANCH_NAME_1,
                        test_repo.get_checked_out_branch_name()?,
                    );
                    Ok(())
                }
            }

            mod when_branch_is_behind {
                use super::*;

                async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
                    // fallback (51,52) user write (53, 55) repo (55, 56)
                    let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                        Relay::new(8051, None, None),
                        Relay::new(8052, None, None),
                        Relay::new(8053, None, None),
                        Relay::new(8055, None, None),
                        Relay::new(8056, None, None),
                    );

                    r51.events.push(generate_test_key_1_relay_list_event());
                    r51.events.push(generate_test_key_1_metadata_event("fred"));
                    r51.events.push(generate_repo_ref_event());

                    r55.events.push(generate_repo_ref_event());
                    r55.events.push(generate_test_key_1_metadata_event("fred"));
                    r55.events.push(generate_test_key_1_relay_list_event());

                    let cli_tester_handle =
                        std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                            let originating_repo = cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            create_and_populate_branch(
                                &test_repo,
                                FEATURE_BRANCH_NAME_1,
                                "a",
                                true,
                            )?;
                            test_repo.checkout("main")?;

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(0, true)?;
                            let mut confirm =
                                p.expect_confirm_eventually("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect_end_eventually_and_print()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok((originating_repo, test_repo))
                        });

                    // launch relay
                    let _ = join!(
                        r51.listen_until_close(),
                        r52.listen_until_close(),
                        r53.listen_until_close(),
                        r55.listen_until_close(),
                        r56.listen_until_close(),
                    );
                    let res = cli_tester_handle.join().unwrap()?;

                    Ok(res)
                }

                mod cli_prompts {
                    use super::*;
                    async fn run_async_prompts_to_choose_from_pr_titles() -> Result<()> {
                        let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                            Relay::new(8051, None, None),
                            Relay::new(8052, None, None),
                            Relay::new(8053, None, None),
                            Relay::new(8055, None, None),
                            Relay::new(8056, None, None),
                        );

                        r51.events.push(generate_test_key_1_relay_list_event());
                        r51.events.push(generate_test_key_1_metadata_event("fred"));
                        r51.events.push(generate_repo_ref_event());

                        r55.events.push(generate_repo_ref_event());
                        r55.events.push(generate_test_key_1_metadata_event("fred"));
                        r55.events.push(generate_test_key_1_relay_list_event());

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            cli_tester_create_prs()?;

                            let test_repo = GitTestRepo::default();
                            test_repo.populate()?;
                            let mut p = CliTester::new_from_dir(&test_repo.dir, ["prs", "list"]);

                            create_and_populate_branch(
                                &test_repo,
                                FEATURE_BRANCH_NAME_1,
                                "a",
                                true,
                            )?;
                            test_repo.checkout("main")?;

                            p.expect("finding PRs...\r\n")?;
                            let mut c = p.expect_choice(
                                "All PRs",
                                vec![
                                    format!("\"{PR_TITLE_1}\""),
                                    format!("\"{PR_TITLE_2}\""),
                                    format!("\"{PR_TITLE_3}\""),
                                ],
                            )?;
                            c.succeeds_with(0, true)?;
                            p.expect("finding commits...\r\n")?;
                            let mut confirm = p.expect_confirm("check out branch?", Some(true))?;
                            confirm.succeeds_with(None)?;
                            p.expect("checked out PR branch. pulled 1 new commits\r\n")?;
                            p.expect_end()?;

                            for p in [51, 52, 53, 55, 56] {
                                relay::shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        });

                        // launch relay
                        let _ = join!(
                            r51.listen_until_close(),
                            r52.listen_until_close(),
                            r53.listen_until_close(),
                            r55.listen_until_close(),
                            r56.listen_until_close(),
                        );
                        cli_tester_handle.join().unwrap()?;
                        println!("{:?}", r55.events);
                        Ok(())
                    }

                    #[tokio::test]
                    #[serial]
                    async fn prompts_to_choose_from_pr_titles() -> Result<()> {
                        let _ = run_async_prompts_to_choose_from_pr_titles().await;
                        Ok(())
                    }
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_checked_out() -> Result<()> {
                    let (_, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        FEATURE_BRANCH_NAME_1,
                        test_repo.get_checked_out_branch_name()?,
                    );
                    Ok(())
                }

                #[tokio::test]
                #[serial]
                async fn pr_branch_tip_is_most_recent_patch() -> Result<()> {
                    let (originating_repo, test_repo) = prep_and_run().await?;
                    assert_eq!(
                        originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                        test_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                    );
                    Ok(())
                }
            }

            mod when_branch_is_ahead {
                // use super::*;
                // TODO latest commit in pr builds off an older commit in pr
                // instead of previous.
                // TODO current git user created commit on branch
            }

            mod when_latest_event_rebases_branch {
                // use super::*;
                // TODO
            }
        }
    }
}
