use anyhow::Result;
use futures::join;
use serial_test::serial;
use test_utils::{git::GitTestRepo, relay::Relay, *};

mod when_main_is_checked_out {
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

                let test_repo = create_repo_with_proposal_branch_pulled_and_checkedout(1)?;

                test_repo.checkout("main")?;

                let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                p.expect("Error: checkout a branch associated with a proposal first\r\n")?;
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

mod when_branch_doesnt_exist {
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

                let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                p.expect("fetching updates...\r\n")?;
                p.expect_eventually("\r\n")?; // some updates listed here
                p.expect("Error: cannot find proposal that matches the current branch name\r\n")?;

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
                    let (_, test_repo) =
                        create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("fetching updates...\r\n")?;
                    p.expect_eventually("\r\n")?; // some updates listed here
                    p.expect("branch already up-to-date\r\n")?;
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
                    let (originating_repo, test_repo) =
                        create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

                    let branch_name =
                        remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main(
                            &test_repo,
                        )?;
                    test_repo.checkout(&branch_name)?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect_end_eventually()?;

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
                        let (originating_repo, test_repo) =
                            create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

                        let branch_name =
                            remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main(
                                &test_repo,
                            )?;
                        test_repo.checkout(&branch_name)?;

                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                        p.expect("fetching updates...\r\n")?;
                        p.expect_eventually("\r\n")?; // some updates listed here
                        p.expect_end_with("applied 1 new commits\r\n")?;

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

        #[tokio::test]
        #[serial]
        async fn proposal_branch_tip_is_most_recent_patch() -> Result<()> {
            let (originating_repo, test_repo) = prep_and_run().await?;
            assert_eq!(
                originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                test_repo.get_tip_of_local_branch(&get_proposal_branch_name(
                    &test_repo,
                    FEATURE_BRANCH_NAME_1
                )?)?,
            );
            Ok(())
        }
    }

    mod when_latest_proposal_amended_locally {
        use super::*;

        mod cli_prompts {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn cli_output_correct() -> Result<()> {
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
                    let (_, test_repo) =
                        create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

                    amend_last_commit(&test_repo, "add ammended-commit.md")?;

                    // run test
                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("fetching updates...\r\n")?;
                    p.expect_eventually("\r\n")?; // some updates listed here
                    p.expect(
                        "you have an amended/rebase version the proposal that is unpublished\r\n",
                    )?;
                    p.expect("you have previously applied the latest version of the proposal (2 ahead 0 behind 'main') but your local proposal branch has amended or rebased it (2 ahead 0 behind 'main')\r\n")?;
                    p.expect("to view the latest proposal but retain your changes:\r\n")?;
                    p.expect("  1) create a new branch off the tip commit of this one to store your changes\r\n")?;
                    p.expect("  2) run `ngit list` and checkout the latest published version of this proposal\r\n")?;
                    p.expect("if you are confident in your changes consider running `ngit push --force`\r\n")?;
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
        }
    }

    mod when_local_commits_on_uptodate_proposal {
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

            let cli_tester_handle = std::thread::spawn(
                move || -> Result<(GitTestRepo, GitTestRepo)> {
                    let (originating_repo, test_repo) =
                        create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

                    // add another commit (so we have a local branch 1 ahead)
                    std::fs::write(test_repo.dir.join("ammended-commit.md"), "some content")?;
                    test_repo.stage_and_commit("add ammended-commit.md")?;

                    // run test
                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("fetching updates...\r\n")?;
                    p.expect_eventually("\r\n")?; // some updates listed here
                    p.expect("local proposal branch exists with 1 unpublished commits on top of the most up-to-date version of the proposal\r\n")?;
                    p.expect_end()?;

                    for p in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + p)?;
                    }
                    Ok((originating_repo, test_repo))
                },
            );

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

            #[tokio::test]
            #[serial]
            async fn prompts_to_choose_from_proposal_titles() -> Result<()> {
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
                    let (_, test_repo) =
                        create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

                    // add another commit (so we have a local branch 1 ahead)
                    std::fs::write(test_repo.dir.join("ammended-commit.md"), "some content")?;
                    test_repo.stage_and_commit("add ammended-commit.md")?;

                    // run test
                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("fetching updates...\r\n")?;
                    p.expect_eventually("\r\n")?; // some updates listed here
                    p.expect("local proposal branch exists with 1 unpublished commits on top of the most up-to-date version of the proposal\r\n")?;
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
        }

        #[tokio::test]
        #[serial]
        async fn didnt_overwrite_local_appendments() -> Result<()> {
            let (originating_repo, test_repo) = prep_and_run().await?;
            assert_ne!(
                test_repo.get_tip_of_local_branch(&get_proposal_branch_name(
                    &test_repo,
                    FEATURE_BRANCH_NAME_1
                )?)?,
                originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            );
            Ok(())
        }
    }
    mod when_latest_event_rebases_branch {
        use tokio::task::JoinHandle;

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

            let cli_tester_handle: JoinHandle<Result<(GitTestRepo, GitTestRepo)>> =
                tokio::task::spawn_blocking(move || {
                    let (originating_repo, test_repo) = create_proposals_with_first_rebased_and_repo_with_latest_main_and_unrebased_proposal()?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
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
            let res = cli_tester_handle.await??;

            Ok(res)
        }

        mod cli_prompts {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn prompts_to_choose_from_proposal_titles() -> Result<()> {
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

                let cli_tester_handle: JoinHandle<Result<()>> = tokio::task::spawn_blocking(
                    move || {
                        let (_, test_repo) = create_proposals_with_first_rebased_and_repo_with_latest_main_and_unrebased_proposal()?;

                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                        p.expect("fetching updates...\r\n")?;
                        p.expect_eventually("\r\n")?; // some updates listed here
                        p.expect_end_with("pulled new version of proposal (2 ahead 0 behind 'main'), replacing old version (2 ahead 1 behind 'main')\r\n")?;
                        for p in [51, 52, 53, 55, 56] {
                            relay::shutdown_relay(8000 + p)?;
                        }
                        Ok(())
                    },
                );

                // launch relay
                let _ = join!(
                    r51.listen_until_close(),
                    r52.listen_until_close(),
                    r53.listen_until_close(),
                    r55.listen_until_close(),
                    r56.listen_until_close(),
                );
                cli_tester_handle.await??;
                println!("{:?}", r55.events);
                Ok(())
            }
        }

        #[tokio::test]
        #[serial]
        async fn proposal_branch_tip_is_most_recent_proposal_revision_tip() -> Result<()> {
            let (originating_repo, test_repo) = prep_and_run().await?;
            assert_eq!(
                originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                test_repo.get_tip_of_local_branch(&get_proposal_branch_name(
                    &test_repo,
                    FEATURE_BRANCH_NAME_1
                )?)?,
            );
            Ok(())
        }
    }
}
