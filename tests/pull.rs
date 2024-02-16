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

    mod cli_prompts {
        use super::*;
        async fn run_async_cli_show_error() -> Result<()> {
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

        #[tokio::test]
        #[serial]
        async fn cli_show_error() -> Result<()> {
            run_async_cli_show_error().await
        }
    }
}

mod when_branch_doesnt_exist {
    use super::*;

    mod cli_prompts {
        use super::*;
        async fn run_async_cli_show_error() -> Result<()> {
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
                p.expect("finding proposal event...\r\n")?;
                p.expect(
                    "Error: cannot find a proposal event associated with the checked out branch name\r\n",
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

        #[tokio::test]
        #[serial]
        async fn cli_show_error() -> Result<()> {
            run_async_cli_show_error().await
        }
    }
}

mod when_branch_is_checked_out {
    use super::*;

    mod when_branch_is_up_to_date {
        use super::*;

        mod cli_prompts {
            use super::*;
            async fn run_async_cli_show_up_to_date() -> Result<()> {
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

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
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

            #[tokio::test]
            #[serial]
            async fn cli_show_up_to_date() -> Result<()> {
                run_async_cli_show_up_to_date().await
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
                    let originating_repo = cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", true)?;

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

            async fn run_async_cli_applied_1_commit() -> Result<()> {
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

                        create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", true)?;

                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                        p.expect("finding proposal root event...\r\n")?;
                        p.expect("found proposal root event. finding commits...\r\n")?;
                        p.expect("applied 1 new commits\r\n")?;
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

            #[tokio::test]
            #[serial]
            async fn cli_applied_1_commit() -> Result<()> {
                run_async_cli_applied_1_commit().await
            }
        }

        #[tokio::test]
        #[serial]
        async fn proposal_branch_tip_is_most_recent_patch() -> Result<()> {
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
        // TODO latest commit in proposal builds off an older commit in proposal
        // instead of previous.
        // TODO current git user created commit on branch
    }

    mod when_latest_event_rebases_branch {
        // use super::*;
        // TODO
    }
}
