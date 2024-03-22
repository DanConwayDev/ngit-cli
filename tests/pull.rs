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
        Some((PROPOSAL_TITLE_1, "proposal a description")),
        None,
    )?;
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_2,
        "b",
        Some((PROPOSAL_TITLE_2, "proposal b description")),
        None,
    )?;
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_3,
        "c",
        Some((PROPOSAL_TITLE_3, "proposal c description")),
        None,
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
    cover_letter_title_and_description: Option<(&str, &str)>,
    in_reply_to: Option<String>,
) -> Result<()> {
    create_and_populate_branch(test_repo, branch_name, prefix, false)?;

    if let Some(in_reply_to) = in_reply_to {
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
                "--no-cover-letter",
                "--in-reply-to",
                in_reply_to.as_str(),
            ],
        );
        p.expect_end_eventually()?;
    } else if let Some((title, description)) = cover_letter_title_and_description {
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
    } else {
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
                "--no-cover-letter",
            ],
        );
        p.expect_end_eventually()?;
    }
    Ok(())
}

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

    mod when_old_proposal_revision_amended_locally {
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
                    cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    // simulating amending an older version of the proposal commits on the current
                    // branch
                    create_and_populate_branch(
                        &test_repo,
                        FEATURE_BRANCH_NAME_1,
                        "a-changed",
                        false,
                    )?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
                    p.expect(
                        "you have an amended/rebase version the proposal that is unpublished\r\n",
                    )?;
                    p.expect("your local proposal branch (2 ahead 0 behind 'main') has conflicting changes with the latest published proposal (2 ahead 0 behind 'main')\r\n")?;
                    p.expect("its likely that you have rebased / amended an old proposal version because git has no record of the latest proposal commit.\r\n")?;
                    p.expect("it is possible that you have been working off the latest version and git has delete this commit as part of a clean up\r\n")?;
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
                    cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    // simulating checking out the proposal (the commits_ids will match)
                    create_and_populate_branch(&test_repo, "different-branch-name", "a", false)?;
                    test_repo.checkout("main")?;
                    // simulating amending the proposal
                    create_and_populate_branch(
                        &test_repo,
                        FEATURE_BRANCH_NAME_1,
                        "a-changed",
                        false,
                    )?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
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
                    let originating_repo = cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
                    // add appended commit to local branch
                    std::fs::write(test_repo.dir.join("appended.md"), "some content")?;
                    test_repo.stage_and_commit("appended commit")?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
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
                    cli_tester_create_proposals()?;

                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;

                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
                    // add appended commit to local branch
                    std::fs::write(test_repo.dir.join("appended.md"), "some content")?;
                    test_repo.stage_and_commit("appended commit")?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect("finding proposal root event...\r\n")?;
                    p.expect("found proposal root event. finding commits...\r\n")?;
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
                test_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
                originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            );
            Ok(())
        }
    }

    mod when_latest_event_rebases_branch {
        use std::time::Duration;

        use nostr_sdk::Client;
        use tokio::{runtime::Handle, task::JoinHandle};

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
                    // create 3 proposals
                    let _ = cli_tester_create_proposals()?;
                    // get proposal id of first
                    let client = Client::default();
                    Handle::current().block_on(client.add_relay("ws://localhost:8055"))?;
                    Handle::current().block_on(client.connect_relay("ws://localhost:8055"))?;
                    let proposals = Handle::current().block_on(client.get_events_of(
                        vec![
                                nostr::Filter::default()
                                    .kind(nostr::Kind::Custom(PATCH_KIND))
                                    .custom_tag(
                                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::T),
                                        vec!["root"],
                                    ),
                            ],
                        Some(Duration::from_millis(500)),
                    ))?;
                    Handle::current().block_on(client.disconnect())?;

                    let proposal_1_id = proposals
                        .iter()
                        .find(|e| {
                            e.tags
                                .iter()
                                .any(|t| t.as_vec()[1].eq(&FEATURE_BRANCH_NAME_1))
                        })
                        .unwrap()
                        .id;
                    // recreate proposal 1 on top of a another commit (like a rebase on top
                    // of one extra commit)
                    let second_originating_repo = GitTestRepo::default();
                    second_originating_repo.populate()?;
                    std::fs::write(
                        second_originating_repo.dir.join("amazing.md"),
                        "some content",
                    )?;
                    second_originating_repo.stage_and_commit("commit for rebasing on top of")?;
                    cli_tester_create_proposal(
                        &second_originating_repo,
                        FEATURE_BRANCH_NAME_1,
                        "a",
                        Some((PROPOSAL_TITLE_1, "proposal a description")),
                        Some(proposal_1_id.to_string()),
                    )?;

                    // pretend we have downloaded the origianl version of the first proposal
                    let test_repo = GitTestRepo::default();
                    test_repo.populate()?;
                    create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
                    // pretend we have pulled the updated main branch
                    test_repo.checkout("main")?;
                    std::fs::write(test_repo.dir.join("amazing.md"), "some content")?;
                    test_repo.stage_and_commit("commit for rebasing on top of")?;
                    test_repo.checkout(FEATURE_BRANCH_NAME_1)?;

                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                    p.expect_end_eventually_and_print()?;

                    for p in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + p)?;
                    }
                    Ok((second_originating_repo, test_repo))
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
                        // create 3 proposals
                        let _ = cli_tester_create_proposals()?;
                        // get proposal id of first
                        let client = Client::default();
                        Handle::current().block_on(client.add_relay("ws://localhost:8055"))?;
                        Handle::current().block_on(client.connect_relay("ws://localhost:8055"))?;
                        let proposals = Handle::current().block_on(client.get_events_of(
                            vec![
                                nostr::Filter::default()
                                    .kind(nostr::Kind::Custom(PATCH_KIND))
                                    .custom_tag(
                                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::T),
                                        vec!["root"],
                                    ),
                            ],
                            Some(Duration::from_millis(500)),
                        ))?;
                        Handle::current().block_on(client.disconnect())?;

                        let proposal_1_id = proposals
                            .iter()
                            .find(|e| {
                                e.tags
                                    .iter()
                                    .any(|t| t.as_vec()[1].eq(&FEATURE_BRANCH_NAME_1))
                            })
                            .unwrap()
                            .id;
                        // recreate proposal 1 on top of a another commit (like a rebase on top
                        // of one extra commit)
                        let second_originating_repo = GitTestRepo::default();
                        second_originating_repo.populate()?;
                        std::fs::write(
                            second_originating_repo.dir.join("amazing.md"),
                            "some content",
                        )?;
                        second_originating_repo
                            .stage_and_commit("commit for rebasing on top of")?;
                        cli_tester_create_proposal(
                            &second_originating_repo,
                            FEATURE_BRANCH_NAME_1,
                            "a",
                            Some((PROPOSAL_TITLE_1, "proposal a description")),
                            Some(proposal_1_id.to_string()),
                        )?;

                        // pretend we have downloaded the origianl version of the first proposal
                        let test_repo = GitTestRepo::default();
                        test_repo.populate()?;
                        create_and_populate_branch(&test_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
                        // pretend we have pulled the updated main branch
                        test_repo.checkout("main")?;
                        std::fs::write(test_repo.dir.join("amazing.md"), "some content")?;
                        test_repo.stage_and_commit("commit for rebasing on top of")?;
                        test_repo.checkout(FEATURE_BRANCH_NAME_1)?;

                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["pull"]);
                        p.expect("finding proposal root event...\r\n")?;
                        p.expect("found proposal root event. finding commits...\r\n")?;
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
                test_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            );
            Ok(())
        }
    }
}
