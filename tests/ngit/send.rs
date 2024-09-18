use anyhow::Result;
use futures::join;
use nostr_sdk::Kind;
use serial_test::serial;
use test_utils::{git::GitTestRepo, relay::Relay, *};

#[test]
fn when_no_main_or_master_branch_return_error() -> Result<()> {
    let test_repo = GitTestRepo::new("notmain")?;
    test_repo.populate()?;
    let mut p = CliTester::new_from_dir(&test_repo.dir, ["send"]);
    p.expect("Error: the default branches (main or master) do not exist")?;
    Ok(())
}

// TODO when commits ahead of origin/master - test ask to proceed
// TODO when commits in origin/master - test ask to proceed
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

    fn expect_confirm_prompt(p: &mut CliTester) -> Result<CliTesterConfirmPrompt> {
        p.expect("fetching updates...\r\n")?;
        p.expect_eventually("\r\n")?; // may be 'no updates' or some updates
        p.expect("creating proposal from 2 commits:\r\n")?;
        p.expect("fe973a8 add t4.md\r\n")?;
        p.expect("232efb3 add t3.md\r\n")?;
        p.expect_confirm(
            "proposal is 1 behind 'main'. consider rebasing before submission. proceed anyway?",
            Some(false),
        )
    }

    #[test]
    fn asked_with_default_no() -> Result<()> {
        let test_repo = prep_test_repo()?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["send", "HEAD~2"]);
        expect_confirm_prompt(&mut p)?;
        p.exit()?;
        Ok(())
    }

    #[test]
    fn when_response_is_false_aborts() -> Result<()> {
        let test_repo = prep_test_repo()?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["send", "HEAD~2"]);

        expect_confirm_prompt(&mut p)?.succeeds_with(Some(false))?;

        p.expect_end_with("Error: aborting so commits can be rebased\r\n")?;

        Ok(())
    }
    #[test]
    #[serial]
    fn when_response_is_true_proceeds() -> Result<()> {
        let test_repo = prep_test_repo()?;

        let mut p = CliTester::new_from_dir(&test_repo.dir, ["send", "HEAD~2"]);
        expect_confirm_prompt(&mut p)?.succeeds_with(Some(true))?;
        p.expect("? include cover letter")?;
        p.exit()?;
        Ok(())
    }
}

fn is_cover_letter(event: &nostr::Event) -> bool {
    event.kind.eq(&Kind::GitPatch)
        && event
            .tags()
            .iter()
            .any(|t| t.as_vec()[1].eq("cover-letter"))
}

fn is_patch(event: &nostr::Event) -> bool {
    event.kind.eq(&Kind::GitPatch)
        && !event
            .tags()
            .iter()
            .any(|t| t.as_vec()[1].eq("cover-letter"))
}

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

fn cli_tester_create_proposal(git_repo: &GitTestRepo, include_cover_letter: bool) -> CliTester {
    let mut args = vec![
        "--nsec",
        TEST_KEY_1_NSEC,
        "--password",
        TEST_PASSWORD,
        "--disable-cli-spinners",
        "send",
        "HEAD~2",
    ];
    if include_cover_letter {
        for arg in [
            "--title",
            "exampletitle",
            "--description",
            "exampledescription",
        ] {
            args.push(arg);
        }
    } else {
        args.push("--no-cover-letter");
    }
    CliTester::new_from_dir(&git_repo.dir, args)
}

fn expect_msgs_first(p: &mut CliTester, include_cover_letter: bool) -> Result<()> {
    p.expect("fetching updates...\r\n")?;
    p.expect_eventually("\r\n")?; // may be 'no updates' or some updates
    p.expect("creating proposal from 2 commits:\r\n")?;
    p.expect("fe973a8 add t4.md\r\n")?;
    p.expect("232efb3 add t3.md\r\n")?;
    // sometimes there will be a 'searching for profile...' msg
    p.expect_eventually("logged in as fred\r\n")?;
    p.expect(format!(
        "posting 2 patches {} a covering letter...\r\n",
        if include_cover_letter {
            "with"
        } else {
            "without"
        }
    ))?;
    Ok(())
}

fn expect_msgs_after(p: &mut CliTester) -> Result<()> {
    p.expect_after_whitespace("view in gitworkshop.dev: https://gitworkshop.dev/repo")?;
    p.expect_eventually("\r\n")?;
    p.expect("view in another client:  https://njump.me/")?;
    p.expect_eventually("\r\n")?;
    Ok(())
}

async fn prep_run_create_proposal(
    include_cover_letter: bool,
) -> Result<(
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
        Relay::new(
            8055,
            None,
            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                relay.respond_events(
                    client_id,
                    &subscription_id,
                    &vec![generate_repo_ref_event()],
                )?;
                Ok(())
            }),
        ),
        Relay::new(8056, None, None),
    );

    // // check relay had the right number of events
    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
        let mut p = cli_tester_create_proposal(&git_repo, include_cover_letter);
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

mod when_cover_letter_details_specified_with_range_of_head_2_sends_cover_letter_and_2_patches_to_3_relays {

    use super::*;
    #[tokio::test]
    #[serial]
    async fn only_1_cover_letter_event_sent_to_each_relay() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
        for relay in [&r53, &r55, &r56] {
            assert_eq!(
                relay.events.iter().filter(|e| is_cover_letter(e)).count(),
                1,
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn only_1_cover_letter_event_sent_to_user_relays() -> Result<()> {
        let (_, _, r53, r55, _) = prep_run_create_proposal(true).await?;
        for relay in [&r53, &r55] {
            assert_eq!(
                relay.events.iter().filter(|e| is_cover_letter(e)).count(),
                1,
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn only_1_cover_letter_event_sent_to_repo_relays() -> Result<()> {
        let (_, _, _, r55, r56) = prep_run_create_proposal(true).await?;
        for relay in [&r55, &r56] {
            assert_eq!(
                relay.events.iter().filter(|e| is_cover_letter(e)).count(),
                1
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn only_1_cover_letter_event_sent_to_fallback_relays() -> Result<()> {
        let (r51, r52, _, _, _) = prep_run_create_proposal(true).await?;
        for relay in [&r51, &r52] {
            assert_eq!(
                relay.events.iter().filter(|e| is_cover_letter(e)).count(),
                1,
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn only_2_patch_kind_events_sent_to_each_relay() -> Result<()> {
        let (r51, r52, r53, r55, r56) = prep_run_create_proposal(true).await?;
        for relay in [&r51, &r52, &r53, &r55, &r56] {
            assert_eq!(relay.events.iter().filter(|e| is_patch(e)).count(), 2,);
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn patch_content_contains_patch_in_email_format_with_patch_series_numbers() -> Result<()>
    {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
        for relay in [&r53, &r55, &r56] {
            let patch_events: Vec<&nostr::Event> =
                relay.events.iter().filter(|e| is_patch(e)).collect();

            assert_eq!(
                patch_events[1].content,
                "\
                    From fe973a840fba2a8ab37dd505c154854a69a6505c Mon Sep 17 00:00:00 2001\n\
                    From: Joe Bloggs <joe.bloggs@pm.me>\n\
                    Date: Thu, 1 Jan 1970 00:00:00 +0000\n\
                    Subject: [PATCH 2/2] add t4.md\n\
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
                    libgit2 1.8.1\n\
                    \n\
                    ",
            );
            assert_eq!(
                patch_events[0].content,
                "\
                    From 232efb37ebc67692c9e9ff58b83c0d3d63971a0a Mon Sep 17 00:00:00 2001\n\
                    From: Joe Bloggs <joe.bloggs@pm.me>\n\
                    Date: Thu, 1 Jan 1970 00:00:00 +0000\n\
                    Subject: [PATCH 1/2] add t3.md\n\
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
                    libgit2 1.8.1\n\
                    \n\
                    ",
            );
        }
        Ok(())
    }

    mod cover_letter_tags {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn root_commit_as_r() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();

                assert_eq!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .find(|t| t.as_vec()[0].eq("r"))
                        .unwrap()
                        .as_vec()[1],
                    "9ee507fc4357d7ee16a5d8901bedcd103f23c17d"
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn a_tag_for_repo_event_of_each_maintainer() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                assert!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .any(|t| t.as_vec()[0].eq("a")
                            && t.as_vec()[1].eq(&format!(
                                "{}:{TEST_KEY_1_PUBKEY_HEX}:{}",
                                Kind::GitRepoAnnouncement,
                                generate_repo_ref_event().identifier().unwrap()
                            )))
                );
                assert!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .any(|t| t.as_vec()[0].eq("a")
                            && t.as_vec()[1].eq(&format!(
                                "{}:{TEST_KEY_2_PUBKEY_HEX}:{}",
                                Kind::GitRepoAnnouncement,
                                generate_repo_ref_event().identifier().unwrap()
                            )))
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn p_tags_for_maintainers() -> Result<()> {
            let event = generate_repo_ref_event();
            let maintainers = &event
                .tags()
                .iter()
                .find(|t| t.as_vec()[0].eq(&"maintainers"))
                .unwrap()
                .as_vec()[1..];
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                for m in maintainers {
                    let cover_letter_event: &nostr::Event =
                        relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                    assert!(
                        cover_letter_event
                            .tags()
                            .iter()
                            .any(|t| { t.as_vec()[0].eq("p") && t.as_vec()[1].eq(m) })
                    );
                }
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn t_tag_cover_letter() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                assert!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .any(|t| { t.as_vec()[0].eq("t") && t.as_vec()[1].eq(&"cover-letter") })
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn t_tag_root() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                assert!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .any(|t| { t.as_vec()[0].eq("t") && t.as_vec()[1].eq(&"root") })
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn cover_letter_tags_branch_name() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();

                // branch-name tag
                assert_eq!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .find(|t| t.as_vec()[0].eq("branch-name"))
                        .unwrap()
                        .as_vec()[1],
                    "feature"
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn cover_letter_tags_alt() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();

                // branch-name tag
                assert_eq!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .find(|t| t.as_vec()[0].eq("alt"))
                        .unwrap()
                        .as_vec()[1],
                    "git patch cover letter: exampletitle"
                );
            }
            Ok(())
        }
    }

    mod patch_tags {
        use super::*;

        async fn prep() -> Result<nostr::Event> {
            let (_, _, r53, _, _) = prep_run_create_proposal(true).await?;
            Ok(r53.events.iter().find(|e| is_patch(e)).unwrap().clone())
        }

        #[tokio::test]
        #[serial]
        async fn commit_and_commit_r() -> Result<()> {
            static COMMIT_ID: &str = "232efb37ebc67692c9e9ff58b83c0d3d63971a0a";
            let most_recent_patch = prep().await?;
            assert!(
                most_recent_patch
                    .tags
                    .iter()
                    .any(|t| t.as_vec()[0].eq("r") && t.as_vec()[1].eq(COMMIT_ID))
            );
            assert!(
                most_recent_patch
                    .tags
                    .iter()
                    .any(|t| t.as_vec()[0].eq("commit") && t.as_vec()[1].eq(COMMIT_ID))
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn parent_commit() -> Result<()> {
            // commit parent 'r' and 'parent-commit' tag
            static COMMIT_PARENT_ID: &str = "431b84edc0d2fa118d63faa3c2db9c73d630a5ae";
            let most_recent_patch = prep().await?;
            assert_eq!(
                most_recent_patch
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("parent-commit"))
                    .unwrap()
                    .as_vec()[1],
                COMMIT_PARENT_ID,
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn root_commit_as_r() -> Result<()> {
            assert!(prep().await?.tags.iter().any(|t| t.as_vec()[0].eq("r")
                && t.as_vec()[1].eq("9ee507fc4357d7ee16a5d8901bedcd103f23c17d")));
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn p_tags_for_maintainers() -> Result<()> {
            let event = generate_repo_ref_event();
            let maintainers = &event
                .tags()
                .iter()
                .find(|t| t.as_vec()[0].eq(&"maintainers"))
                .unwrap()
                .as_vec()[1..];
            for m in maintainers {
                assert!(
                    prep()
                        .await?
                        .tags()
                        .iter()
                        .any(|t| { t.as_vec()[0].eq("p") && t.as_vec()[1].eq(m) })
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn a_tag_for_repo_event_of_each_maintainer() -> Result<()> {
            assert!(prep().await?.tags.iter().any(|t| {
                t.as_vec()[0].eq("a")
                    && t.as_vec()[1].eq(&format!(
                        "{}:{TEST_KEY_1_PUBKEY_HEX}:{}",
                        Kind::GitRepoAnnouncement,
                        generate_repo_ref_event().identifier().unwrap()
                    ))
            }));
            assert!(prep().await?.tags.iter().any(|t| {
                t.as_vec()[0].eq("a")
                    && t.as_vec()[1].eq(&format!(
                        "{}:{TEST_KEY_2_PUBKEY_HEX}:{}",
                        Kind::GitRepoAnnouncement,
                        generate_repo_ref_event().identifier().unwrap()
                    ))
            }));
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn description_with_commit_message() -> Result<()> {
            assert_eq!(
                prep()
                    .await?
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("description"))
                    .unwrap()
                    .as_vec()[1],
                "add t3.md"
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn commit_author() -> Result<()> {
            assert_eq!(
                prep()
                    .await?
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("author"))
                    .unwrap()
                    .as_vec(),
                vec!["author", "Joe Bloggs", "joe.bloggs@pm.me", "0", "0"],
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn commit_committer() -> Result<()> {
            assert_eq!(
                prep()
                    .await?
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("committer"))
                    .unwrap()
                    .as_vec(),
                vec!["committer", "Joe Bloggs", "joe.bloggs@pm.me", "0", "0"],
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn alt() -> Result<()> {
            assert_eq!(
                prep()
                    .await?
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("alt"))
                    .unwrap()
                    .as_vec(),
                vec!["alt", "git patch: add t3.md"],
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn patch_tags_cover_letter_event_as_root() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let patch_events: Vec<&nostr::Event> =
                    relay.events.iter().filter(|e| is_patch(e)).collect();

                let most_recent_patch = patch_events[0];
                let cover_letter_event = relay.events.iter().find(|e| is_cover_letter(e)).unwrap();

                let root_event_tag = most_recent_patch
                    .tags
                    .iter()
                    .find(|t| {
                        t.as_vec()[0].eq("e") && t.as_vec().len().eq(&4) && t.as_vec()[3].eq("root")
                    })
                    .unwrap();

                assert_eq!(
                    root_event_tag.as_vec()[1],
                    cover_letter_event.id.to_string()
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn second_patch_tags_first_with_reply() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal(true).await?;
            for relay in [&r53, &r55, &r56] {
                let patch_events = relay
                    .events
                    .iter()
                    .filter(|e| is_patch(e))
                    .collect::<Vec<&nostr::Event>>();
                assert_eq!(
                    patch_events[1]
                        .tags()
                        .iter()
                        .find(|t| t.as_vec()[0].eq("e")
                            && t.as_vec().len().eq(&4)
                            && t.as_vec()[3].eq("reply"))
                        .unwrap()
                        .as_vec()[1],
                    patch_events[0].id.to_string(),
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn no_t_root_tag() -> Result<()> {
            assert!(
                !prep()
                    .await?
                    .tags
                    .iter()
                    .any(|t| t.as_vec()[0].eq("t") && t.as_vec()[1].eq("root"))
            );
            Ok(())
        }
    }
    mod cli_ouput {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn check_cli_output() -> Result<()> {
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
                Relay::new(
                    8055,
                    None,
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(
                            client_id,
                            &subscription_id,
                            &vec![generate_repo_ref_event()],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            // // check relay had the right number of events
            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_create_proposal(&git_repo, true);
                expect_msgs_first(&mut p, true)?;
                relay::expect_send_with_progress(
                    &mut p,
                    vec![
                        (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                        (" [my-relay] ws://localhost:8053", true, ""),
                        (" [repo-relay] ws://localhost:8056", true, ""),
                        (" [default] ws://localhost:8051", true, ""),
                        (" [default] ws://localhost:8052", true, ""),
                    ],
                    3,
                )?;
                expect_msgs_after(&mut p)?;
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
    }

    mod first_event_rejected_by_1_relay {
        use super::*;

        mod only_first_rejected_event_sent_to_relay {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn only_first_rejected_event_sent_to_relay() -> Result<()> {
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
                    Relay::new(
                        8055,
                        None,
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_repo_ref_event()],
                            )?;
                            Ok(())
                        }),
                    ),
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
                    let mut p = cli_tester_create_proposal(&git_repo, true);
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
        }

        mod cli_show_rejection_with_comment {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn cli_show_rejection_with_comment() -> Result<()> {
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
                    Relay::new(
                        8055,
                        None,
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_repo_ref_event()],
                            )?;
                            Ok(())
                        }),
                    ),
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
                    let mut p = cli_tester_create_proposal(&git_repo, true);
                    expect_msgs_first(&mut p, true)?;
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
                            (" [default] ws://localhost:8051", true, ""),
                            (" [default] ws://localhost:8052", true, ""),
                        ],
                        3,
                    )?;
                    expect_msgs_after(&mut p)?;
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
        }
    }
}

mod when_no_cover_letter_flag_set_with_range_of_head_2_sends_2_patches_without_cover_letter {
    use super::*;

    mod cli_ouput {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn check_cli_output() -> Result<()> {
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
                Relay::new(
                    8055,
                    None,
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(
                            client_id,
                            &subscription_id,
                            &vec![generate_repo_ref_event()],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            // // check relay had the right number of events
            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_create_proposal(&git_repo, false);

                expect_msgs_first(&mut p, false)?;
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
                expect_msgs_after(&mut p)?;
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
    }

    #[tokio::test]
    #[serial]
    async fn no_cover_letter_event() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(false).await?;
        for relay in [&r53, &r55, &r56] {
            assert_eq!(
                relay.events.iter().filter(|e| is_cover_letter(e)).count(),
                0,
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn two_patch_events() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(false).await?;
        for relay in [&r53, &r55, &r56] {
            assert_eq!(relay.events.iter().filter(|e| is_patch(e)).count(), 2);
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    // TODO check this is the ancestor
    async fn first_patch_with_root_t_tag() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(false).await?;
        for relay in [&r53, &r55, &r56] {
            let patch_events = relay
                .events
                .iter()
                .filter(|e| is_patch(e))
                .collect::<Vec<&nostr::Event>>();

            // first patch tagged as root
            assert!(
                patch_events[0]
                    .tags()
                    .iter()
                    .any(|t| t.as_vec()[0].eq("t") && t.as_vec()[1].eq("root"))
            );
            // second patch not tagged as root
            assert!(
                !patch_events[1]
                    .tags()
                    .iter()
                    .any(|t| t.as_vec()[0].eq("t") && t.as_vec()[1].eq("root"))
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn root_patch_tags_branch_name() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(false).await?;
        for relay in [&r53, &r55, &r56] {
            let patch_events = relay
                .events
                .iter()
                .filter(|e| is_patch(e))
                .collect::<Vec<&nostr::Event>>();

            // branch-name tag
            assert_eq!(
                patch_events[0]
                    .tags()
                    .iter()
                    .find(|t| t.as_vec()[0].eq("branch-name"))
                    .unwrap()
                    .as_vec()[1],
                "feature"
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn second_patch_lists_first_as_root() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal(false).await?;
        for relay in [&r53, &r55, &r56] {
            let patch_events = relay
                .events
                .iter()
                .filter(|e| is_patch(e))
                .collect::<Vec<&nostr::Event>>();

            assert_eq!(
                patch_events[1]
                    .tags()
                    .iter()
                    .find(|t| t.as_vec()[0].eq("e")
                        && t.as_vec().len().eq(&4)
                        && t.as_vec()[3].eq("root"))
                    .unwrap()
                    .as_vec()[1],
                patch_events[0].id.to_string(),
            );
        }
        Ok(())
    }
}

mod when_range_ommited_prompts_for_selection_defaulting_ahead_of_main {
    use super::*;

    fn cli_tester_create_proposal(git_repo: &GitTestRepo) -> CliTester {
        let args = vec![
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "send",
            "--no-cover-letter",
        ];
        CliTester::new_from_dir(&git_repo.dir, args)
    }
    fn expect_msgs_first(p: &mut CliTester) -> Result<()> {
        p.expect("fetching updates...\r\n")?;
        p.expect_eventually("\r\n")?; // may be 'no updates' or some updates
        let mut selector = p.expect_multi_select(
            "select commits for proposal",
            vec![
                "(Joe Bloggs) add t4.md [feature] fe973a8".to_string(),
                "(Joe Bloggs) add t3.md 232efb3".to_string(),
                "(Joe Bloggs) add t2.md [main] 431b84e".to_string(),
                "(Joe Bloggs) add t1.md af474d8".to_string(),
                "(Joe Bloggs) Initial commit 9ee507f".to_string(),
            ],
        )?;
        selector.succeeds_with(vec![0, 1], false, vec![0, 1])?;
        p.expect("creating proposal from 2 commits:\r\n")?;
        p.expect("fe973a8 add t4.md\r\n")?;
        p.expect("232efb3 add t3.md\r\n")?;
        p.expect("searching for profile...\r\n")?;
        p.expect("logged in as fred\r\n")?;
        p.expect("posting 2 patches without a covering letter...\r\n")?;
        Ok(())
    }
    async fn prep_run_create_proposal() -> Result<(
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
            Relay::new(
                8055,
                None,
                Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                    relay.respond_events(
                        client_id,
                        &subscription_id,
                        &vec![generate_repo_ref_event()],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8056, None, None),
        );

        // // check relay had the right number of events
        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_create_proposal(&git_repo);
            expect_msgs_first(&mut p)?;
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
    mod cli_ouput {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn check_cli_output() -> Result<()> {
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
                Relay::new(
                    8055,
                    None,
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(
                            client_id,
                            &subscription_id,
                            &vec![generate_repo_ref_event()],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_create_proposal(&git_repo);

                expect_msgs_first(&mut p)?;
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
                expect_msgs_after(&mut p)?;
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
    }

    #[tokio::test]
    #[serial]
    async fn two_patch_events_sent() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
        for relay in [&r53, &r55, &r56] {
            assert_eq!(relay.events.iter().filter(|e| is_patch(e)).count(), 2);
        }
        Ok(())
    }
}

mod root_proposal_specified_using_in_reply_to_with_range_of_head_2_and_cover_letter_details_specified {

    use nostr::ToBech32;

    use super::*;

    fn cli_tester_create_proposal(git_repo: &GitTestRepo) -> CliTester {
        let proposal_root_bech32 = get_pretend_proposal_root_event().id.to_bech32().unwrap();
        let args = vec![
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "send",
            "HEAD~2",
            "--in-reply-to",
            &proposal_root_bech32,
            "--title",
            "exampletitle",
            "--description",
            "exampledescription",
        ];
        CliTester::new_from_dir(&git_repo.dir, args)
    }
    fn expect_msgs_first(p: &mut CliTester, include_cover_letter: bool) -> Result<()> {
        p.expect("fetching updates...\r\n")?;
        p.expect("updates: 1 new maintainer, 1 announcement update, 1 proposal\r\n")?;
        let proposal_root_bech32 = get_pretend_proposal_root_event().id.to_bech32().unwrap();
        p.expect(format!(
            "creating proposal revision for: {}\r\n",
            proposal_root_bech32,
        ))?;
        p.expect("creating proposal from 2 commits:\r\n")?;
        p.expect("fe973a8 add t4.md\r\n")?;
        p.expect("232efb3 add t3.md\r\n")?;
        p.expect("logged in as fred\r\n")?;
        p.expect(format!(
            "posting 2 patches {} a covering letter...\r\n",
            if include_cover_letter {
                "with"
            } else {
                "without"
            }
        ))?;
        Ok(())
    }

    async fn prep_run_create_proposal() -> Result<(
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
                            get_pretend_proposal_root_event(),
                        ],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8052, None, None),
            Relay::new(8053, None, None),
            Relay::new(
                8055,
                None,
                Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                    relay.respond_events(
                        client_id,
                        &subscription_id,
                        &vec![generate_repo_ref_event(), get_pretend_proposal_root_event()],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8056, None, None),
        );

        // // check relay had the right number of events
        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_create_proposal(&git_repo);
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
    mod cli_ouput {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn check_cli_output() -> Result<()> {
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
                                get_pretend_proposal_root_event(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8052, None, None),
                Relay::new(8053, None, None),
                Relay::new(
                    8055,
                    None,
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(
                            client_id,
                            &subscription_id,
                            &vec![generate_repo_ref_event(), get_pretend_proposal_root_event()],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            // // check relay had the right number of events
            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_create_proposal(&git_repo);
                expect_msgs_first(&mut p, true)?;
                relay::expect_send_with_progress(
                    &mut p,
                    vec![
                        (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                        (" [my-relay] ws://localhost:8053", true, ""),
                        (" [repo-relay] ws://localhost:8056", true, ""),
                        (" [default] ws://localhost:8051", true, ""),
                        (" [default] ws://localhost:8052", true, ""),
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
    }

    mod cover_letter_tags {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn t_tag_root() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                assert!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .any(|t| { t.as_vec()[0].eq("t") && t.as_vec()[1].eq(&"root") })
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn t_tag_revision_root() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                assert!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .any(|t| { t.as_vec()[0].eq("t") && t.as_vec()[1].eq(&"revision-root") })
                );
            }
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn e_tag_in_reply_to_event_as_reply() -> Result<()> {
            let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
            for relay in [&r53, &r55, &r56] {
                let cover_letter_event: &nostr::Event =
                    relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
                assert_eq!(
                    cover_letter_event
                        .tags()
                        .iter()
                        .find(|t| {
                            t.as_vec()[0].eq("e")
                                && t.as_vec().len().eq(&4)
                                && t.as_vec()[3].eq("reply")
                        })
                        .unwrap()
                        .as_vec()[1],
                    // id of state nevent
                    "431e58eb8e1b4e20292d1d5bbe81d5cfb042e1bc165de32eddfdd52245a4cce4",
                );
            }
            Ok(())
        }
    }

    #[tokio::test]
    #[serial]
    async fn patch_tags_cover_letter_event_as_root() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
        for relay in [&r53, &r55, &r56] {
            let patch_events: Vec<&nostr::Event> =
                relay.events.iter().filter(|e| is_patch(e)).collect();

            let cover_letter_event = relay.events.iter().find(|e| is_cover_letter(e)).unwrap();

            for patch in patch_events {
                assert_eq!(
                    patch
                        .tags
                        .iter()
                        .find(|t| {
                            t.as_vec()[0].eq("e")
                                && t.as_vec().len().eq(&4)
                                && t.as_vec()[3].eq("root")
                        })
                        .unwrap()
                        .as_vec()[1],
                    cover_letter_event.id.to_string()
                );
            }
        }
        Ok(())
    }
}

mod in_reply_to_mentions_issue {
    use nostr::ToBech32;

    use super::*;
    pub fn get_pretend_issue_event() -> nostr::Event {
        serde_json::from_str(r#"{"created_at":1709286372,"content":"please provide feedback\nthis is an example ngit issue to demonstrate gitworkshop.dev.\n\nplease provide feedback with in reply to this issue or by creating a new issue.","tags":[["r","26689f97810fc656c7134c76e2a37d33b2e40ce7"],["a","30617:a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d:ngit","wss://relay.damus.io","root"],["p","a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d"]],"kind":1621,"pubkey":"a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d","id":"e944765d625ae7323d080da0df069c726a0e5490a17b452f854d85e18f781588","sig":"a1af9e89a35f1f7ef93e3de33986bd86cb7c4d7d9abb233c0c6405f32b5788171e47f84551afe8515b3107d12f03472721ea784b8791ff3f25e66a3169a54c20"}"#).unwrap()
    }

    fn cli_tester_create_proposal(git_repo: &GitTestRepo) -> CliTester {
        let issue_bech32 = get_pretend_issue_event().id.to_bech32().unwrap();
        let args = vec![
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "send",
            "HEAD~2",
            "--in-reply-to",
            &issue_bech32,
            // "note1a9z8vhtzttnny0ggpksd7p5uwf4qu4ys59a52tu9fkz7rrmczkyqc46ngg",
            "--title",
            "exampletitle",
            "--description",
            "exampledescription",
        ];
        CliTester::new_from_dir(&git_repo.dir, args)
    }

    async fn prep_run_create_proposal() -> Result<(
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
                            get_pretend_issue_event(),
                        ],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8052, None, None),
            Relay::new(8053, None, None),
            Relay::new(
                8055,
                None,
                Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                    relay.respond_events(
                        client_id,
                        &subscription_id,
                        &vec![generate_repo_ref_event(), get_pretend_issue_event()],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8056, None, None),
        );

        // // check relay had the right number of events
        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_create_proposal(&git_repo);
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

    #[tokio::test]
    #[serial]
    async fn issue_event_mentioned_in_tagged_cover_letter() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
        for relay in [&r53, &r55, &r56] {
            let cover_letter_event: &nostr::Event =
                relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
            assert!(cover_letter_event.tags().iter().any(|t| {
                t.as_vec()[0].eq("e")
                    && t.as_vec()[1].eq(&get_pretend_issue_event().id.to_hex())
                    && t.as_vec()[3].eq(&"mention")
            }));
        }
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn isnt_tagged_as_revision() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
        for relay in [&r53, &r55, &r56] {
            let cover_letter_event: &nostr::Event =
                relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
            assert!(
                !cover_letter_event
                    .tags()
                    .iter()
                    .any(|t| { t.as_vec()[0].eq("t") && t.as_vec()[1].eq(&"revision-root") })
            );
        }
        Ok(())
    }
}
mod in_reply_to_mentions_npub_and_nprofile_which_get_mentioned_in_proposal_root {

    use super::*;

    fn cli_tester_create_proposal(git_repo: &GitTestRepo) -> CliTester {
        let args = vec![
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "send",
            "HEAD~2",
            "--in-reply-to",
            // nsec1q3c5xnsm5m4wgsrhwnz04p0d5mevkryyggqgdpa9jwulpq9gldhswgtxvq
            "npub1knxeegzqg0xqflsryvg7l7x7nmpe7kd7pl7zazug0a7t99tdsphszuyapx",
            // nsec1nx5ulvcndhcuu8k6q8fenw50l6y75sec7pj8vr0r68l6a44w3lqspvj02k
            "nprofile1qqsvru3yqrec6dxjn06f8cjh79jcu9wyaxu4y6v47yzpsx7vjm4xcuc33z2n3",
            "--title",
            "exampletitle",
            "--description",
            "exampledescription",
        ];
        CliTester::new_from_dir(&git_repo.dir, args)
    }

    async fn prep_run_create_proposal() -> Result<(
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
            Relay::new(
                8055,
                None,
                Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                    relay.respond_events(
                        client_id,
                        &subscription_id,
                        &vec![generate_repo_ref_event()],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8056, None, None),
        );

        // // check relay had the right number of events
        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_create_proposal(&git_repo);
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

    #[tokio::test]
    #[serial]
    async fn npub_and_nprofile_mentioned_in_tagged_cover_letter() -> Result<()> {
        let (_, _, r53, r55, r56) = prep_run_create_proposal().await?;
        for relay in [&r53, &r55, &r56] {
            let cover_letter_event: &nostr::Event =
                relay.events.iter().find(|e| is_cover_letter(e)).unwrap();
            assert!(cover_letter_event.tags().iter().any(|t| {
                t.as_vec()[0].eq("p")
                    && t.as_vec()[1].eq(&nostr::Keys::parse(
                        "nsec1q3c5xnsm5m4wgsrhwnz04p0d5mevkryyggqgdpa9jwulpq9gldhswgtxvq",
                    )
                    .unwrap()
                    .public_key()
                    .to_hex())
            }));
            assert!(cover_letter_event.tags().iter().any(|t| {
                t.as_vec()[0].eq("p")
                    && t.as_vec()[1].eq(&nostr::Keys::parse(
                        "nsec1nx5ulvcndhcuu8k6q8fenw50l6y75sec7pj8vr0r68l6a44w3lqspvj02k",
                    )
                    .unwrap()
                    .public_key()
                    .to_hex())
            }));
        }
        Ok(())
    }
}
