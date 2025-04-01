use anyhow::Result;
use nostr_sdk::Kind;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

fn expect_msgs_first(p: &mut CliTester) -> Result<()> {
    p.expect("searching for profile...\r\n")?;
    p.expect("logged in as fred via cli arguments\r\n")?;
    // // p.expect("searching for existing claims on repository...\r\n")?;
    p.expect("publishing repostory reference...\r\n")?;
    Ok(())
}

fn expect_prompt_to_set_origin(p: &mut CliTester) -> Result<()> {
    p.expect_confirm_eventually(
        "set remote \"origin\" to the nostr url of your repository?",
        Some(true),
    )?
    .succeeds_with(Some(false))?;
    Ok(())
}

fn get_cli_args() -> Vec<&'static str> {
    vec![
        "--nsec",
        TEST_KEY_1_NSEC,
        "--password",
        TEST_PASSWORD,
        "--disable-cli-spinners",
        "init",
        "--title",
        "example-name",
        "--identifier",
        "example-identifier",
        "--description",
        "example-description",
        "--web",
        "https://exampleproject.xyz",
        "https://gitworkshop.dev/123",
        "--relays",
        "ws://localhost:8055",
        "ws://localhost:8056",
        "--clone-url",
        "https://git.myhosting.com/my-repo.git",
        "--earliest-unique-commit",
        "9ee507fc4357d7ee16a5d8901bedcd103f23c17d",
        "--other-maintainers",
        TEST_KEY_1_NPUB,
    ]
}

mod when_repo_not_previously_claimed {
    use super::*;

    mod when_repo_relays_specified_as_arguments {
        use futures::join;
        use test_utils::relay::Relay;

        use super::*;

        fn prep_git_repo() -> Result<GitTestRepo> {
            let test_repo = GitTestRepo::without_repo_in_git_config();
            test_repo.populate()?;
            test_repo.add_remote("origin", "https://localhost:1000")?;
            Ok(test_repo)
        }

        fn cli_tester_init(git_repo: &GitTestRepo) -> CliTester {
            CliTester::new_from_dir(&git_repo.dir, get_cli_args())
        }

        async fn prep_run_init() -> Result<(
            Relay<'static>,
            Relay<'static>,
            Relay<'static>,
            Relay<'static>,
            Relay<'static>,
            Relay<'static>,
        )> {
            let git_repo = prep_git_repo()?;
            // fallback (51,52) user write (53, 55) repo (55, 56) blaster (57)
            let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
                Relay::new(
                    8051,
                    None,
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(client_id, &subscription_id, &vec![
                            generate_test_key_1_metadata_event("fred"),
                            generate_test_key_1_relay_list_event(),
                        ])?;
                        Ok(())
                    }),
                ),
                Relay::new(8052, None, None),
                Relay::new(8053, None, None),
                Relay::new(8055, None, None),
                Relay::new(8056, None, None),
                Relay::new(8057, None, None),
            );

            // // check relay had the right number of events
            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_init(&git_repo);
                expect_prompt_to_set_origin(&mut p)?;
                p.expect_end_eventually()?;
                for p in [51, 52, 53, 55, 56, 57] {
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
                r57.listen_until_close(),
            );
            cli_tester_handle.join().unwrap()?;
            Ok((r51, r52, r53, r55, r56, r57))
        }

        mod sent_to_correct_relays {

            use super::*;

            #[tokio::test]
            #[serial]
            async fn only_1_repository_kind_event_sent_to_user_relays() -> Result<()> {
                let (_, _, r53, r55, _, _) = prep_run_init().await?;
                for relay in [&r53, &r55] {
                    assert_eq!(
                        relay
                            .events
                            .iter()
                            .filter(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                            .count(),
                        1,
                    );
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn only_1_repository_kind_event_sent_to_specified_repo_relays() -> Result<()> {
                let (_, _, _, r55, r56, _) = prep_run_init().await?;
                for relay in [&r55, &r56] {
                    assert_eq!(
                        relay
                            .events
                            .iter()
                            .filter(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                            .count(),
                        1,
                    );
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn only_1_repository_kind_event_sent_to_fallback_relays() -> Result<()> {
                let (r51, r52, _, _, _, _) = prep_run_init().await?;
                for relay in [&r51, &r52] {
                    assert_eq!(
                        relay
                            .events
                            .iter()
                            .filter(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                            .count(),
                        1,
                    );
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn only_1_repository_kind_event_sent_to_blaster_relays() -> Result<()> {
                let (_, _, _, _, _, r57) = prep_run_init().await?;
                assert_eq!(
                    r57.events
                        .iter()
                        .filter(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .count(),
                    1,
                );
                Ok(())
            }
        }

        mod git_config_updated {

            use nostr::nips::{nip01::Coordinate, nip19::Nip19Coordinate};
            use nostr_sdk::ToBech32;

            use super::*;

            async fn async_run_test() -> Result<()> {
                let git_repo = prep_git_repo()?;
                // fallback (51,52) user write (53, 55) repo (55, 56) blaster (57)
                let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
                    Relay::new(
                        8051,
                        None,
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                            ])?;
                            Ok(())
                        }),
                    ),
                    Relay::new(8052, None, None),
                    Relay::new(8053, None, None),
                    Relay::new(8055, None, None),
                    Relay::new(8056, None, None),
                    Relay::new(8057, None, None),
                );

                // // check relay had the right number of events
                let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                    let mut p = cli_tester_init(&git_repo);
                    expect_prompt_to_set_origin(&mut p)?;
                    p.expect_end_eventually()?;
                    for p in [51, 52, 53, 55, 56, 57] {
                        relay::shutdown_relay(8000 + p)?;
                    }
                    assert_eq!(
                        git_repo
                            .git_repo
                            .config()?
                            .get_entry("nostr.repo")?
                            .value()
                            .unwrap(),
                        Nip19Coordinate {
                            coordinate: Coordinate {
                                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                                identifier: "example-identifier".to_string(),
                                public_key: TEST_KEY_1_KEYS.public_key(),
                            },
                            relays: vec![],
                        }
                        .to_bech32()?,
                    );

                    Ok(())
                });

                // launch relay
                let _ = join!(
                    r51.listen_until_close(),
                    r52.listen_until_close(),
                    r53.listen_until_close(),
                    r55.listen_until_close(),
                    r56.listen_until_close(),
                    r57.listen_until_close(),
                );
                cli_tester_handle.join().unwrap()?;
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn with_nostr_repo_set_to_user_and_identifer_naddr() -> Result<()> {
                async_run_test().await?;
                Ok(())
            }
        }

        mod tags_as_specified_in_args {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn d_replaceable_event_identifier() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();

                    assert!(event.tags.iter().any(
                        |t| t.as_slice()[0].eq("d") && t.as_slice()[1].eq("example-identifier")
                    ));
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn earliest_unique_commit_as_reference_with_euc_marker() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();

                    assert!(event.tags.iter().any(|t| t.as_slice()[0].eq("r")
                        && t.as_slice()[1].eq("9ee507fc4357d7ee16a5d8901bedcd103f23c17d")
                        && t.as_slice()[2].eq("euc")));
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn name() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();

                    assert!(
                        event
                            .tags
                            .iter()
                            .any(|t| t.as_slice()[0].eq("name")
                                && t.as_slice()[1].eq("example-name"))
                    );
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn alt() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();

                    assert!(event.tags.iter().any(|t| t.as_slice()[0].eq("alt")
                        && t.as_slice()[1].eq("git repository: example-name")));
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn description() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();

                    assert!(event.tags.iter().any(|t| t.as_slice()[0].eq("description")
                        && t.as_slice()[1].eq("example-description")));
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn git_server() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();

                    assert!(
                        event.tags.iter().any(|t| t.as_slice()[0].eq("clone")
                            && t.as_slice()[1].eq("https://git.myhosting.com/my-repo.git")) /* todo check it defaults to origin */
                    );
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn relays() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();
                    let relays_tag = event
                        .tags
                        .iter()
                        .find(|t| t.as_slice()[0].eq("relays"))
                        .unwrap()
                        .as_slice();
                    assert_eq!(relays_tag[1], "ws://localhost:8055",);
                    assert_eq!(relays_tag[2], "ws://localhost:8056",);
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn web() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();
                    let web_tag = event
                        .tags
                        .iter()
                        .find(|t| t.as_slice()[0].eq("web"))
                        .unwrap()
                        .as_slice();
                    assert_eq!(web_tag[1], "https://exampleproject.xyz",);
                    assert_eq!(web_tag[2], "https://gitworkshop.dev/123",);
                }
                Ok(())
            }

            #[tokio::test]
            #[serial]
            async fn maintainers() -> Result<()> {
                let (_, _, r53, r55, r56, r57) = prep_run_init().await?;
                for relay in [&r53, &r55, &r56, &r57] {
                    let event: &nostr::Event = relay
                        .events
                        .iter()
                        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
                        .unwrap();
                    let maintainers_tag = event
                        .tags
                        .iter()
                        .find(|t| t.as_slice()[0].eq("maintainers"))
                        .unwrap()
                        .as_slice();
                    assert_eq!(maintainers_tag[1], TEST_KEY_1_KEYS.public_key().to_string());
                }
                Ok(())
            }
        }

        mod cli_ouput {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn check_cli_output() -> Result<()> {
                let git_repo = prep_git_repo()?;

                // fallback (51,52) user write (53, 55) repo (55, 56) blaster (57)
                let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
                    Relay::new(
                        8051,
                        None,
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                            ])?;
                            Ok(())
                        }),
                    ),
                    Relay::new(8052, None, None),
                    Relay::new(8053, None, None),
                    Relay::new(8055, None, None),
                    Relay::new(8056, None, None),
                    Relay::new(8057, None, None),
                );

                // // check relay had the right number of events
                let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                    let mut p = cli_tester_init(&git_repo);
                    expect_msgs_first(&mut p)?;
                    relay::expect_send_with_progress(
                        &mut p,
                        vec![
                            (" [my-relay] [repo-relay] ws://localhost:8055", true, ""),
                            (" [my-relay] ws://localhost:8053", true, ""),
                            (" [repo-relay] ws://localhost:8056", true, ""),
                            (" [default] ws://localhost:8051", true, ""),
                            (" [default] ws://localhost:8052", true, ""),
                            (" [default] ws://localhost:8057", true, ""),
                        ],
                        1,
                    )?;
                    expect_prompt_to_set_origin(&mut p)?;
                    p.expect_end_eventually()?;
                    for p in [51, 52, 53, 55, 56, 57] {
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
                    r57.listen_until_close(),
                );
                cli_tester_handle.join().unwrap()?;
                Ok(())
            }
        }
    }
    // TODO: cli caputuring input
}
// TODO: when_updating_existing_repoistory correct defaults are used
