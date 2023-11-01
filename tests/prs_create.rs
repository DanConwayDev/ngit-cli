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

#[test]
#[serial]
fn cli_message_creating_patches() -> Result<()> {
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

mod sends_pr_and_2_patches_to_3_relays {
    use futures::join;
    use test_utils::relay::Relay;

    use super::*;

    static PR_KIND: u64 = 318;
    static PATCH_KIND: u64 = 317;

    fn prep_git_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::default();
        test_repo.populate()?;
        // create feature branch with 2 commit ahead
        test_repo.create_branch("feature")?;
        test_repo.checkout("feature")?;
        std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
        test_repo.stage_and_commit("add t3.md")?;
        std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
        test_repo.stage_and_commit("add t4.md")?;
        Ok(test_repo)
    }

    fn cli_tester_create_pr(git_repo: &GitTestRepo) -> CliTester {
        CliTester::new_from_dir(
            &git_repo.dir,
            [
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
                "--disable-cli-spinners",
                "prs",
                "create",
                "--title",
                "example",
                "--description",
                "example",
            ],
        )
    }

    fn expect_msgs_first(p: &mut CliTester) -> Result<()> {
        p.expect("creating patch for 2 commits from 'head' that can be merged into 'main'\r\n")?;
        p.expect("searching for your details...\r\n")?;
        p.expect("\r")?;
        p.expect("logged in as fred\r\n")?;
        p.expect("posting 1 pull request with 2 commits...\r\n")?;
        Ok(())
    }

    async fn prep_run_create_pr() -> Result<(
        Relay<'static>,
        Relay<'static>,
        Relay<'static>,
        Relay<'static>,
        Relay<'static>,
    )> {
        let git_repo = prep_git_repo()?;
        // fallback (51,52) user write (53, 55) repo (55, 56)
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
            Relay::new(
                8051,
                None,
                Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                    relay.respond_events(
                        client_id,
                        &subscription_id,
                        &vec![
                            generate_test_key_1_metadata_event("fred"),
                            generate_test_key_1_relay_list_event(),
                        ],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8052, None, None),
            Relay::new(8053, None, None),
            Relay::new(8055, None, None),
            Relay::new(8056, None, None),
        );

        // // check relay had the right number of events
        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_create_pr(&git_repo);
            p.expect_end_eventually()?;
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
        Ok((r51, r52, r53, r55, r56))
    }

    #[test]
    #[serial]
    fn only_1_pr_kind_event_sent_to_each_relay() -> Result<()> {
        let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
        for relay in [&r53, &r55, &r56] {
            assert_eq!(
                relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PR_KIND))
                    .count(),
                1,
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn only_1_pr_kind_event_sent_to_user_relays() -> Result<()> {
        let (_, _, r53, r55, _) = futures::executor::block_on(prep_run_create_pr())?;
        for relay in [&r53, &r55] {
            assert_eq!(
                relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PR_KIND))
                    .count(),
                1,
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn only_1_pr_kind_event_sent_to_repo_relays() -> Result<()> {
        let (_, _, _, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
        for relay in [&r55, &r56] {
            assert_eq!(
                relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PR_KIND))
                    .count(),
                1,
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn pr_not_sent_to_fallback_relay() -> Result<()> {
        let (r51, r52, _, _, _) = futures::executor::block_on(prep_run_create_pr())?;
        for relay in [&r51, &r52] {
            assert_eq!(
                relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PR_KIND))
                    .count(),
                0,
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn only_2_patch_kind_events_sent_to_each_relay() -> Result<()> {
        let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
        for relay in [&r53, &r55, &r56] {
            assert_eq!(
                relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PATCH_KIND))
                    .count(),
                2,
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn patch_content_contains_patch_in_email_format() -> Result<()> {
        let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
        for relay in [&r53, &r55, &r56] {
            let patch_events: Vec<&nostr::Event> = relay
                .events
                .iter()
                .filter(|e| e.kind.as_u64().eq(&PATCH_KIND))
                .collect();

            assert_eq!(
                patch_events[0].content,
                "\
                    From fe973a840fba2a8ab37dd505c154854a69a6505c Mon Sep 17 00:00:00 2001\n\
                    From: Joe Bloggs <joe.bloggs@pm.me>\n\
                    Date: Thu, 1 Jan 1970 00:00:00 +0000\n\
                    Subject: [PATCH] add t4.md\n\
                    \n\
                    ---\n \
                    t4.md | 1 +\n \
                    1 file changed, 1 insertion(+)\n \
                    create mode 100644 t4.md\n\
                    \n\
                    diff --git a/t4.md b/t4.md\n\
                    new file mode 100644\n\
                    index 0000000..f0eec86\n\
                    --- /dev/null\n\
                    +++ b/t4.md\n\
                    @@ -0,0 +1 @@\n\
                    +some content\n\\ \
                    No newline at end of file\n\
                    --\n\
                    libgit2 1.7.1\n\
                    \n\
                    ",
            );
            assert_eq!(
                patch_events[1].content,
                "\
                    From 232efb37ebc67692c9e9ff58b83c0d3d63971a0a Mon Sep 17 00:00:00 2001\n\
                    From: Joe Bloggs <joe.bloggs@pm.me>\n\
                    Date: Thu, 1 Jan 1970 00:00:00 +0000\n\
                    Subject: [PATCH] add t3.md\n\
                    \n\
                    ---\n \
                    t3.md | 1 +\n \
                    1 file changed, 1 insertion(+)\n \
                    create mode 100644 t3.md\n\
                    \n\
                    diff --git a/t3.md b/t3.md\n\
                    new file mode 100644\n\
                    index 0000000..f0eec86\n\
                    --- /dev/null\n\
                    +++ b/t3.md\n\
                    @@ -0,0 +1 @@\n\
                    +some content\n\\ \
                    No newline at end of file\n\
                    --\n\
                    libgit2 1.7.1\n\
                    \n\
                    ",
            );
        }
        Ok(())
    }

    mod pr_tags {
        use super::*;
        #[test]
        #[serial]
        fn pr_tags_repo_commit() -> Result<()> {
            let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
            for relay in [&r53, &r55, &r56] {
                let pr_event: &nostr::Event = relay
                    .events
                    .iter()
                    .find(|e| e.kind.as_u64().eq(&PR_KIND))
                    .unwrap();

                // root commit 't' tag
                assert!(pr_event.tags.iter().any(|t| t.as_vec()[0].eq("t")
                    && t.as_vec()[1].eq("r-9ee507fc4357d7ee16a5d8901bedcd103f23c17d")));
            }
            Ok(())
        }
    }

    mod patch_tags {
        use super::*;
        #[test]
        #[serial]
        fn patch_tags_correctly_formatted() -> Result<()> {
            let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
            for relay in [&r53, &r55, &r56] {
                let patch_events: Vec<&nostr::Event> = relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PATCH_KIND))
                    .collect();

                static COMMIT_ID: &str = "fe973a840fba2a8ab37dd505c154854a69a6505c";
                let most_recent_patch = patch_events[0];

                // commit 't' and 'commit' tag
                assert!(
                    most_recent_patch
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("t") && t.as_vec()[1].eq(COMMIT_ID))
                );
                assert!(
                    most_recent_patch
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("commit") && t.as_vec()[1].eq(COMMIT_ID))
                );

                // commit parent 't' and 'parent-commit' tag
                static COMMIT_PARENT_ID: &str = "232efb37ebc67692c9e9ff58b83c0d3d63971a0a";
                assert!(
                    most_recent_patch
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("t") && t.as_vec()[1].eq(COMMIT_PARENT_ID))
                );
                assert!(most_recent_patch.tags.iter().any(
                    |t| t.as_vec()[0].eq("parent-commit") && t.as_vec()[1].eq(COMMIT_PARENT_ID)
                ));

                // root commit 't' tag
                assert!(most_recent_patch.tags.iter().any(|t| t.as_vec()[0].eq("t")
                    && t.as_vec()[1].eq("r-9ee507fc4357d7ee16a5d8901bedcd103f23c17d")));
            }
            Ok(())
        }

        #[test]
        #[serial]
        fn patch_tags_pr_event_as_root() -> Result<()> {
            let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_create_pr())?;
            for relay in [&r53, &r55, &r56] {
                let patch_events: Vec<&nostr::Event> = relay
                    .events
                    .iter()
                    .filter(|e| e.kind.as_u64().eq(&PATCH_KIND))
                    .collect();

                let most_recent_patch = patch_events[0];
                let pr_event = relay
                    .events
                    .iter()
                    .find(|e| e.kind.as_u64().eq(&PR_KIND))
                    .unwrap();

                let root_event_tag = most_recent_patch
                    .tags
                    .iter()
                    .find(|t| {
                        t.as_vec()[0].eq("e") && t.as_vec().len().eq(&4) && t.as_vec()[3].eq("root")
                    })
                    .unwrap();

                assert_eq!(root_event_tag.as_vec()[1], pr_event.id.to_string());
            }
            Ok(())
        }
    }
    mod cli_ouput {
        use super::*;

        async fn run_test_async() -> Result<()> {
            let git_repo = prep_git_repo()?;

            let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                Relay::new(
                    8051,
                    None,
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(
                            client_id,
                            &subscription_id,
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8052, None, None),
                Relay::new(8053, None, None),
                Relay::new(8055, None, None),
                Relay::new(8056, None, None),
            );

            // // check relay had the right number of events
            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_create_pr(&git_repo);
                expect_msgs_first(&mut p)?;
                relay::expect_send_with_progress(
                    &mut p,
                    vec![
                        (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                        (" [my-relay] ws://localhost:8053", true, ""),
                        (" [repo-relay] ws://localhost:8056", true, ""),
                    ],
                    3,
                )?;
                p.expect_end_with_whitespace()?;
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

        #[test]
        #[serial]
        fn check_cli_output() -> Result<()> {
            futures::executor::block_on(run_test_async())?;
            Ok(())
        }
    }

    mod first_event_rejected_by_1_relay {
        use super::*;

        mod only_first_rejected_event_sent_to_relay {
            use super::*;

            async fn run_test_async() -> Result<()> {
                let git_repo = prep_git_repo()?;

                let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                    Relay::new(
                        8051,
                        None,
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event(),
                                ],
                            )?;
                            Ok(())
                        }),
                    ),
                    Relay::new(8052, None, None),
                    Relay::new(8053, None, None),
                    Relay::new(8055, None, None),
                    Relay::new(
                        8056,
                        Some(&|relay, client_id, event| -> Result<()> {
                            relay.respond_ok(client_id, event, Some("Payment Required"))?;
                            Ok(())
                        }),
                        None,
                    ),
                );

                // // check relay had the right number of events
                let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                    let mut p = cli_tester_create_pr(&git_repo);
                    p.expect_end_eventually()?;
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

                assert_eq!(r56.events.len(), 1);

                Ok(())
            }

            #[test]
            #[serial]
            fn only_first_rejected_event_sent_to_relay() -> Result<()> {
                futures::executor::block_on(run_test_async())?;
                Ok(())
            }
        }

        mod cli_show_rejection_with_comment {
            use super::*;

            async fn run_test_async() -> Result<(Relay<'static>, Relay<'static>, Relay<'static>)> {
                let git_repo = prep_git_repo()?;

                let (mut r51, mut r52, mut r53, mut r55, mut r56) = (
                    Relay::new(
                        8051,
                        None,
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event(),
                                ],
                            )?;
                            Ok(())
                        }),
                    ),
                    Relay::new(8052, None, None),
                    Relay::new(8053, None, None),
                    Relay::new(8055, None, None),
                    Relay::new(
                        8056,
                        Some(&|relay, client_id, event| -> Result<()> {
                            relay.respond_ok(client_id, event, Some("Payment Required"))?;
                            Ok(())
                        }),
                        None,
                    ),
                );

                // // check relay had the right number of events
                let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                    let mut p = cli_tester_create_pr(&git_repo);
                    expect_msgs_first(&mut p)?;
                    // p.expect_end_with("bla")?;
                    relay::expect_send_with_progress(
                        &mut p,
                        vec![
                            (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                            (" [my-relay] ws://localhost:8053", true, ""),
                            (
                                " [repo-relay] ws://localhost:8056",
                                false,
                                "error: Payment Required",
                            ),
                        ],
                        3,
                    )?;
                    p.expect_end_with_whitespace()?;
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
                Ok((r51, r52, r53))
            }

            #[test]
            #[serial]
            fn cli_show_rejection_with_comment() -> Result<()> {
                futures::executor::block_on(run_test_async())?;
                Ok(())
            }
        }
    }
}
