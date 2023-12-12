use anyhow::Result;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

#[test]
fn when_no_main_or_master_branch_return_error() -> Result<()> {
    let test_repo = GitTestRepo::new("notmain")?;
    test_repo.populate()?;
    let mut p = CliTester::new_from_dir(&test_repo.dir, ["claim"]);
    p.expect("Error: no main or master branch")?;
    Ok(())
}

fn expect_msgs_first(p: &mut CliTester) -> Result<()> {
    p.expect("searching for your details...\r\n")?;
    p.expect("\r")?;
    p.expect("logged in as fred\r\n")?;
    // // p.expect("searching for existing claims on repository...\r\n")?;
    p.expect("maintainers.yaml created. commit and push.\r\n")?;
    p.expect("publishing repostory reference...\r\n")?;
    Ok(())
}

mod when_repo_not_previously_claimed {
    use super::*;

    mod when_repo_relays_specified_as_arguments {
        use futures::join;
        use test_utils::relay::Relay;

        use super::*;

        fn prep_git_repo() -> Result<GitTestRepo> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            test_repo.add_remote("origin", "https://localhost:1000")?;
            Ok(test_repo)
        }

        fn cli_tester_claim(git_repo: &GitTestRepo) -> CliTester {
            CliTester::new_from_dir(
                &git_repo.dir,
                [
                    "--nsec",
                    TEST_KEY_1_NSEC,
                    "--password",
                    TEST_PASSWORD,
                    "--disable-cli-spinners",
                    "claim",
                    "--title",
                    "example-name",
                    "--description",
                    "example-description",
                    "--relays",
                    "ws://localhost:8055",
                    "ws://localhost:8056",
                ],
            )
        }

        async fn prep_run_claim() -> Result<(
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
                let mut p = cli_tester_claim(&git_repo);
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

        mod sent_to_correct_relays {
            use super::*;

            #[test]
            #[serial]
            fn only_1_repository_kind_event_sent_to_user_relays() -> Result<()> {
                let (_, _, r53, r55, _) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55] {
                    assert_eq!(
                        relay
                            .events
                            .iter()
                            .filter(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                            .count(),
                        1,
                    );
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn only_1_repository_kind_event_sent_to_specified_repo_relays() -> Result<()> {
                let (_, _, _, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r55, &r56] {
                    assert_eq!(
                        relay
                            .events
                            .iter()
                            .filter(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                            .count(),
                        1,
                    );
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn event_not_sent_to_fallback_relay() -> Result<()> {
                let (r51, r52, _, _, _) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r51, &r52] {
                    assert_eq!(
                        relay
                            .events
                            .iter()
                            .filter(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                            .count(),
                        0,
                    );
                }
                Ok(())
            }
        }

        mod yaml_file {
            use std::{fs, io::Read};

            use super::*;

            async fn async_run_test() -> Result<()> {
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
                    let mut p = cli_tester_claim(&git_repo);
                    p.expect_end_eventually()?;

                    let yaml_path = git_repo.dir.join("maintainers.yaml");

                    assert!(yaml_path.exists());

                    let mut file = fs::File::open(yaml_path).expect("no such file");
                    let mut file_contents = "".to_string();
                    let _ = file.read_to_string(&mut file_contents);
                    assert_eq!(
                        file_contents,
                        format!(
                            "\
                        maintainers:\n\
                        - {TEST_KEY_1_NPUB}\n\
                        relays:\n\
                        - ws://localhost:8055\n\
                        - ws://localhost:8056\n\
                        "
                        ),
                    );
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
            fn contains_maintainers_and_relays() -> Result<()> {
                futures::executor::block_on(async_run_test())?;
                Ok(())
            }
        }

        mod tags {
            use super::*;

            #[test]
            #[serial]
            fn root_commit_as_d_replaceable_event_identifier() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    assert!(event.tags.iter().any(|t| t.as_vec()[0].eq("d")
                        && t.as_vec()[1].eq("9ee507fc4357d7ee16a5d8901bedcd103f23c17d")));
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn root_commit_as_reference() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    // root commit 'r' tag with 'r-' prefix
                    assert!(event.tags.iter().any(|t| t.as_vec()[0].eq("r")
                        && t.as_vec()[1].eq("r-9ee507fc4357d7ee16a5d8901bedcd103f23c17d")));
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn name() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    assert!(
                        event
                            .tags
                            .iter()
                            .any(|t| t.as_vec()[0].eq("name") && t.as_vec()[1].eq("example-name"))
                    );
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn description() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    assert!(event.tags.iter().any(|t| t.as_vec()[0].eq("description")
                        && t.as_vec()[1].eq("example-description")));
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn git_server() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    assert!(event.tags.iter().any(|t| t.as_vec()[0].eq("git-server")
                        && t.as_vec()[1].eq("https://localhost:1000")));
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn git_server_as_reference() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    assert!(event.tags.iter().any(
                        |t| t.as_vec()[0].eq("r") && t.as_vec()[1].eq("https://localhost:1000")
                    ));
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn relays() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    let relay_tags = event
                        .tags
                        .iter()
                        .filter(|t| t.as_vec()[0].eq("relay"))
                        .collect::<Vec<&nostr::Tag>>();
                    assert_eq!(relay_tags[0].as_vec()[1], "ws://localhost:8055");
                    assert_eq!(relay_tags[1].as_vec()[1], "ws://localhost:8056");
                }
                Ok(())
            }

            #[test]
            #[serial]
            fn current_user_tagged_indicating_maintainer() -> Result<()> {
                let (_, _, r53, r55, r56) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55, &r56] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    let relay_tags = event
                        .tags
                        .iter()
                        .filter(|t| t.as_vec()[0].eq("p"))
                        .collect::<Vec<&nostr::Tag>>();
                    assert_eq!(relay_tags.len(), 1);
                    assert_eq!(
                        relay_tags[0].as_vec()[1],
                        TEST_KEY_1_KEYS.public_key().to_string()
                    );
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
                    let mut p = cli_tester_claim(&git_repo);
                    expect_msgs_first(&mut p)?;
                    relay::expect_send_with_progress(
                        &mut p,
                        vec![
                            (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                            (" [my-relay] ws://localhost:8053", true, ""),
                            (" [repo-relay] ws://localhost:8056", true, ""),
                        ],
                        1,
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
    }

    mod when_repo_relays_not_specified {
        use futures::join;
        use test_utils::relay::Relay;

        use super::*;

        fn prep_git_repo() -> Result<GitTestRepo> {
            let test_repo = GitTestRepo::default();
            test_repo.populate()?;
            test_repo.add_remote("origin", "https://localhost:1000")?;
            Ok(test_repo)
        }

        fn cli_tester_claim(git_repo: &GitTestRepo) -> CliTester {
            CliTester::new_from_dir(
                &git_repo.dir,
                [
                    "--nsec",
                    TEST_KEY_1_NSEC,
                    "--password",
                    TEST_PASSWORD,
                    "--disable-cli-spinners",
                    "claim",
                    "--title",
                    "example-name",
                    "--description",
                    "example-description",
                ],
            )
        }

        async fn prep_run_claim() -> Result<(
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
                let mut p = cli_tester_claim(&git_repo);
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

        mod tags {
            use super::*;

            #[test]
            #[serial]
            fn relays_match_user_write_relays() -> Result<()> {
                let (_, _, r53, r55, _) = futures::executor::block_on(prep_run_claim())?;
                for relay in [&r53, &r55] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.as_u64().eq(&REPOSITORY_KIND))
                        .unwrap();

                    let relay_tags = event
                        .tags
                        .iter()
                        .filter(|t| t.as_vec()[0].eq("relay"))
                        .collect::<Vec<&nostr::Tag>>();
                    assert_eq!(relay_tags[0].as_vec()[1], "ws://localhost:8053");
                    assert_eq!(relay_tags[1].as_vec()[1], "ws://localhost:8055");
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
                    let mut p = cli_tester_claim(&git_repo);
                    expect_msgs_first(&mut p)?;
                    relay::expect_send_with_progress(
                        &mut p,
                        vec![
                            (" [my-relay] [repo-relay] ws://localhost:8053", true, ""),
                            (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                        ],
                        1,
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
    }
}

// TODO: when_updating_existing_repoistory
