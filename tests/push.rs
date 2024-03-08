use anyhow::Result;
use futures::join;
use serial_test::serial;
use test_utils::{git::GitTestRepo, relay::Relay, *};

static FEATURE_BRANCH_NAME_1: &str = "feature-example-t";
static FEATURE_BRANCH_NAME_2: &str = "feature-example-f";
static FEATURE_BRANCH_NAME_3: &str = "feature-example-c";

static PROPOSAL_TITLE_1: &str = "proposal a";
static PROPOSAL_TITLE_2: &str = "proposal b";
static PROPOSAL_TITLE_3: &str = "proposal c";

fn cli_tester_create_proposals() -> Result<GitTestRepo> {
    let git_repo = GitTestRepo::default();
    git_repo.populate()?;
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_1,
        "a",
        PROPOSAL_TITLE_1,
        "proposal a description",
    )?;
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_2,
        "b",
        PROPOSAL_TITLE_2,
        "proposal b description",
    )?;
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_3,
        "c",
        PROPOSAL_TITLE_3,
        "proposal c description",
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

fn cli_tester_create_proposal(
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
            "send",
            "HEAD~2",
            "--title",
            format!("\"{title}\"").as_str(),
            "--description",
            format!("\"{description}\"").as_str(),
        ],
    );
    p.expect_end_eventually()?;
    Ok(())
}

mod when_main_is_checked_out {
    use super::*;

    #[test]
    fn cli_returns_error() -> Result<()> {
        let test_repo = GitTestRepo::default();
        test_repo.populate()?;
        create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
        test_repo.checkout("main")?;
        let mut p = CliTester::new_from_dir(&test_repo.dir, ["push"]);
        p.expect("Error: checkout a branch associated with a proposal first\r\n")?;
        p.expect_end()?;
        Ok(())
    }
}

mod when_proposal_isnt_associated_with_branch_name {
    use super::*;

    mod cli_prompts {

        use super::*;

        #[tokio::test]
        #[serial]
        async fn cli_show_error() -> Result<()> {
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
                cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                test_repo.create_branch("random-name")?;
                test_repo.checkout("random-name")?;

                let mut p = CliTester::new_from_dir(&test_repo.dir, ["push"]);
                p.expect("finding proposal root event...\r\n")?;
                p.expect(
                    "Error: cannot find a proposal root event associated with the checked out branch name\r\n",
                )?;

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
            Ok(())
        }
    }
}

mod when_branch_is_checked_out {
    use super::*;

    mod when_branch_is_up_to_date {
        use super::*;

        mod cli_prompts {
            use super::*;
            #[tokio::test]
            #[serial]
            async fn cli_show_up_to_date() -> Result<()> {
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
                    cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["push"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
                    p.expect("Error: proposal already up-to-date with local branch\r\n")?;
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
                Ok(())
            }
        }
    }

    mod when_branch_is_behind {
        use super::*;

        mod cli_prompts {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn cli_show_proposal_ahead_error() -> Result<()> {
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
                    cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", true)?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["push"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
                    p.expect("Error: proposal is ahead of local branch\r\n")?;
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
                Ok(())
            }
        }
    }

    mod when_branch_is_ahead {
        use super::*;

        mod cli_prompts {
            use test_utils::relay::expect_send_with_progress;

            use super::*;

            #[tokio::test]
            #[serial]
            async fn cli_applied_1_commit() -> Result<()> {
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
                        let originating_repo = cli_tester_create_proposals()?;

                        let test_repo = GitTestRepo::default();
                        test_repo.populate()?;

                        create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;

                        std::fs::write(test_repo.dir.join("a5.md"), "some content")?;
                        test_repo.stage_and_commit("add a5.md".to_string().as_str())?;

                        let mut p = CliTester::new_from_dir(
                            &test_repo.dir,
                            [
                                "--nsec",
                                TEST_KEY_1_NSEC,
                                "--password",
                                TEST_PASSWORD,
                                "--disable-cli-spinners",
                                "push",
                            ],
                        );
                        p.expect("finding proposal root event...\r\n")?;
                        p.expect("found proposal root event. finding commits...\r\n")?;
                        p.expect(
                            "1 commits ahead. preparing to create creating patch events.\r\n",
                        )?;
                        p.expect("searching for profile and relay updates...\r\n")?;
                        p.expect("\r")?;
                        p.expect("logged in as fred\r\n")?;
                        p.expect("pushing 1 commits\r\n")?;

                        expect_send_with_progress(
                            &mut p,
                            vec![
                                (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                                (" [my-relay] ws://localhost:8053", true, ""),
                                (" [repo-relay] ws://localhost:8056", true, ""),
                                (" [default] ws://localhost:8051", true, ""),
                                (" [default] ws://localhost:8052", true, ""),
                            ],
                            1,
                        )?;
                        p.expect_eventually("pushed 1 commits\r\n")?;
                        p.expect_end()?;

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
                cli_tester_handle.join().unwrap()?;

                Ok(())
            }
        }

        async fn prep_and_run() -> Result<(GitTestRepo, Vec<nostr::Event>)> {
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

            let cli_tester_handle = std::thread::spawn(move || -> Result<GitTestRepo> {
                cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;

                let mut p = CliTester::new_from_dir(
                    &test_repo.dir,
                    [
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--password",
                        TEST_PASSWORD,
                        "--disable-cli-spinners",
                        "push",
                    ],
                );
                p.expect_end_eventually()?;

                for p in [51, 52, 53, 55, 56] {
                    relay::shutdown_relay(8000 + p)?;
                }
                Ok(test_repo)
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

            Ok((res, r55.events.clone()))
        }
        #[tokio::test]
        #[serial]
        async fn commits_issued_as_patch_event() -> Result<()> {
            let (test_repo, r55_events) = prep_and_run().await?;

            let commit_id = test_repo
                .get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?
                .to_string();
            assert!(r55_events.iter().any(|e| {
                e.tags
                    .iter()
                    .any(|t| t.as_vec()[0].eq("commit") && t.as_vec()[1].eq(&commit_id))
            }));
            Ok(())
        }
    }

    mod when_branch_has_been_rebased {
        use super::*;

        mod cli_prompts {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn cli_shows_unpublished_rebase_error() -> Result<()> {
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
                    cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    // simulate rebase
                    std::fs::write(test_repo.dir.join("amazing.md"), "some content")?;
                    test_repo.stage_and_commit("commit for rebasing on top of")?;
                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", true)?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["push"]);
                    // p.expect_end_eventually_and_print()?;

                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
                    p.expect("Error: local unpublished proposal has been rebased. consider force pushing\r\n")?;
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
                Ok(())
            }
        }
        mod with_force_flag {
            use super::*;

            mod cli_prompts {
                use super::*;

                #[tokio::test]
                #[serial]
                async fn cli_shows_revision_sent() -> Result<()> {
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
                        cli_tester_create_proposals()?;

                        let test_repo = GitTestRepo::default();
                        test_repo.populate()?;

                        // simulate rebase
                        std::fs::write(test_repo.dir.join("amazing.md"), "some content")?;
                        test_repo.stage_and_commit("commit for rebasing on top of")?;
                        create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
                        let mut p = CliTester::new_from_dir(
                            &test_repo.dir,
                            [
                                "--nsec",
                                TEST_KEY_1_NSEC,
                                "--password",
                                TEST_PASSWORD,
                                "--disable-cli-spinners",
                                "push",
                                "--force",
                                "--no-cover-letter",
                            ],
                        );
                        p.expect("finding proposal root event...\r\n")?;
                        p.expect("found proposal root event. finding commits...\r\n")?;
                        p.expect("preparing to force push proposal revision...\r\n")?;
                        // standard output from `ngit send`
                        p.expect("creating proposal revision for: ")?;
                        // proposal id will be printed in this gap
                        p.expect_eventually("\r\n")?;
                        let mut selector = p.expect_multi_select(
                            "select commits for proposal",
                            vec![
                                "(Joe Bloggs) add a4.md [feature-example-t] 355bdf1".to_string(),
                                "(Joe Bloggs) add a3.md dbd1115".to_string(),
                                "(Joe Bloggs) commit for rebasing on top of [main] 1aa2cfe"
                                    .to_string(),
                                "(Joe Bloggs) add t2.md 431b84e".to_string(),
                                "(Joe Bloggs) add t1.md af474d8".to_string(),
                                "(Joe Bloggs) Initial commit 9ee507f".to_string(),
                            ],
                        )?;
                        selector.succeeds_with(vec![0, 1], false, vec![0, 1])?;
                        p.expect("creating proposal from 2 commits:\r\n")?;
                        p.expect("355bdf1 add a4.md\r\n")?;
                        p.expect("dbd1115 add a3.md\r\n")?;
                        p.expect("searching for profile and relay updates...\r\n")?;
                        p.expect("\r")?;
                        p.expect("logged in as fred\r\n")?;
                        p.expect("posting 2 patches without a covering letter...\r\n")?;

                        relay::expect_send_with_progress(
                            &mut p,
                            vec![
                                (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                                (" [my-relay] ws://localhost:8053", true, ""),
                                (" [repo-relay] ws://localhost:8056", true, ""),
                                (" [default] ws://localhost:8051", true, ""),
                                (" [default] ws://localhost:8052", true, ""),
                            ],
                            2,
                        )?;
                        // end standard `ngit send output`
                        p.expect_after_whitespace("force pushed proposal revision\r\n")?;
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
                    Ok(())
                }
            }
        }
    }
}
