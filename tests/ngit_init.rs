use anyhow::Result;
use nostr::Event;
use nostr_sdk::Kind;
use rstest::*;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the GitRepoAnnouncement event from a relay's collected events.
fn get_announcement(events: &[Event]) -> &Event {
    events
        .iter()
        .find(|e| e.kind.eq(&Kind::GitRepoAnnouncement))
        .expect("GitRepoAnnouncement event not found")
}

/// Get the first value of a single-value tag (e.g. "d", "name", "description").
fn get_tag_value<'a>(event: &'a Event, tag_name: &str) -> &'a str {
    event
        .tags
        .iter()
        .find(|t| t.as_slice()[0] == tag_name)
        .map(|t| t.as_slice()[1].as_str())
        .unwrap_or_else(|| panic!("tag '{tag_name}' not found"))
}

/// Get all values of a multi-value tag (e.g. "relays", "web", "maintainers",
/// "clone"). Returns slice starting from index 1 (skipping the tag name).
fn get_tag_values(event: &Event, tag_name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice()[0] == tag_name)
        .map(|t| t.as_slice()[1..].iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// State A: Fresh (no coordinate)
// ---------------------------------------------------------------------------

mod state_a_fresh {
    use super::*;

    fn prep_git_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::without_repo_in_git_config();
        test_repo.populate()?;
        test_repo.add_remote("origin", "https://localhost:1000")?;
        Ok(test_repo)
    }

    mod errors {
        use super::*;

        #[test]
        #[serial]
        fn bare_no_flags() -> Result<()> {
            let git_repo = prep_git_repo()?;
            let args = vec!["--nsec", TEST_KEY_1_NSEC, "--disable-cli-spinners", "init"];
            let mut p = CliTester::new_from_dir(&git_repo.dir, args);
            p.expect_eventually("logged in as")?;
            p.expect_eventually("missing required fields")?;
            p.expect_eventually("--name <NAME>")?;
            p.expect_eventually("--grasp-server")?;
            Ok(())
        }

        #[test]
        #[serial]
        fn name_only_missing_server_infra() -> Result<()> {
            let git_repo = prep_git_repo()?;
            let args = vec![
                "--nsec",
                TEST_KEY_1_NSEC,
                "--disable-cli-spinners",
                "init",
                "--name",
                "My Project",
            ];
            let mut p = CliTester::new_from_dir(&git_repo.dir, args);
            p.expect_eventually("logged in as")?;
            p.expect_eventually("missing --grasp-server")?;
            Ok(())
        }

        #[test]
        #[serial]
        fn relays_only_missing_name_and_servers() -> Result<()> {
            let git_repo = prep_git_repo()?;
            let args = vec![
                "--nsec",
                TEST_KEY_1_NSEC,
                "--disable-cli-spinners",
                "init",
                "--relays",
                "ws://localhost:8055",
            ];
            let mut p = CliTester::new_from_dir(&git_repo.dir, args);
            p.expect_eventually("logged in as")?;
            p.expect_eventually("missing required fields")?;
            p.expect_eventually("--name <NAME>")?;
            p.expect_eventually("--grasp-server")?;
            Ok(())
        }
    }

    mod success {
        use futures::join;
        use test_utils::relay::Relay;

        use super::*;

        async fn run_init_with_grasp_server(
            extra_args: Vec<&str>,
        ) -> Result<(nostr::Event, GitTestRepo)> {
            let git_repo = prep_git_repo()?;
            let (mut r51, mut r52, mut r53, mut r55) = (
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
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                let extra_args_owned: Vec<String> =
                    extra_args.iter().map(|s| s.to_string()).collect();
                move || -> Result<()> {
                    let mut args =
                        vec!["--nsec", TEST_KEY_1_NSEC, "--disable-cli-spinners", "init"];
                    let extra_refs: Vec<&str> =
                        extra_args_owned.iter().map(|s| s.as_str()).collect();
                    args.extend(extra_refs);
                    let mut p = CliTester::new_from_dir(&dir, args);
                    p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(())
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
            );
            cli_tester_handle.join().unwrap()?;

            let event = get_announcement(&r53.events).clone();
            Ok((event, git_repo))
        }

        mod with_name_and_grasp_server {
            use super::*;

            #[fixture]
            async fn scenario() -> (nostr::Event, GitTestRepo) {
                run_init_with_grasp_server(vec![
                    "--name",
                    "My Project",
                    "--grasp-server",
                    "ws://localhost:8055",
                ])
                .await
                .expect("init failed")
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn identifier_derived_from_name(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                assert_eq!(get_tag_value(&event, "d"), "My-Project");
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn name_tag_matches(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                assert_eq!(get_tag_value(&event, "name"), "My Project");
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn description_empty(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                assert_eq!(get_tag_value(&event, "description"), "");
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn clone_url_derived_from_grasp_server(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                let clone_urls = get_tag_values(&event, "clone");
                assert_eq!(clone_urls.len(), 1);
                assert!(
                    clone_urls[0].starts_with("http://localhost:8055/"),
                    "clone url should start with grasp server: {}",
                    clone_urls[0]
                );
                assert!(
                    clone_urls[0].ends_with("/My-Project.git"),
                    "clone url should end with identifier.git: {}",
                    clone_urls[0]
                );
                assert!(
                    clone_urls[0].contains(TEST_KEY_1_NPUB),
                    "clone url should contain npub: {}",
                    clone_urls[0]
                );
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn relays_include_grasp_derived(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                let relays = get_tag_values(&event, "relays");
                assert!(
                    relays.contains(&"ws://localhost:8055".to_string()),
                    "relays should include grasp-derived relay: {:?}",
                    relays
                );
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn maintainers_is_just_me(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                let maintainers = get_tag_values(&event, "maintainers");
                assert_eq!(maintainers.len(), 1);
                assert_eq!(maintainers[0], TEST_KEY_1_KEYS.public_key().to_string());
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn earliest_unique_commit_is_root(
                #[future] scenario: (nostr::Event, GitTestRepo),
            ) -> Result<()> {
                let (event, _) = scenario.await;
                let euc_tag = event
                    .tags
                    .iter()
                    .find(|t| {
                        t.as_slice()[0] == "r" && t.as_slice().len() > 2 && t.as_slice()[2] == "euc"
                    })
                    .expect("euc tag not found");
                assert_eq!(
                    euc_tag.as_slice()[1],
                    "9ee507fc4357d7ee16a5d8901bedcd103f23c17d"
                );
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State B: Coordinate exists, no announcement found
// ---------------------------------------------------------------------------

mod state_b_coordinate_only {
    use super::*;

    fn prep_git_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::default();
        test_repo.populate()?;
        test_repo.add_remote("origin", "https://localhost:1000")?;
        Ok(test_repo)
    }

    mod errors {
        use futures::join;
        use test_utils::relay::Relay;

        use super::*;

        async fn run_init_expecting_error(extra_args: Vec<&str>) -> Result<String> {
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
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                let extra_args_owned: Vec<String> =
                    extra_args.iter().map(|s| s.to_string()).collect();
                move || -> Result<String> {
                    let mut args =
                        vec!["--nsec", TEST_KEY_1_NSEC, "--disable-cli-spinners", "init"];
                    let extra_refs: Vec<&str> =
                        extra_args_owned.iter().map(|s| s.as_str()).collect();
                    args.extend(extra_refs);
                    let mut p = CliTester::new_from_dir(&dir, args);
                    let output = p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(output)
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
            );
            cli_tester_handle.join().unwrap()
        }

        #[tokio::test]
        #[serial]
        async fn bare_no_flags() -> Result<()> {
            let output = run_init_expecting_error(vec![]).await?;
            assert!(
                output.contains("no announcement found for coordinate"),
                "expected coordinate error, got: {output}"
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn defaults_still_requires_force() -> Result<()> {
            let output = run_init_expecting_error(vec!["--defaults"]).await?;
            assert!(
                output.contains("no announcement found for coordinate"),
                "expected coordinate error even with -d, got: {output}"
            );
            Ok(())
        }
    }

    mod success {
        use futures::join;
        use test_utils::relay::Relay;

        use super::*;

        #[fixture]
        async fn state_b_force() -> nostr::Event {
            let git_repo = prep_git_repo().expect("prep failed");
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
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                move || -> Result<()> {
                    let args = vec![
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--disable-cli-spinners",
                        "init",
                        "--force",
                        "--grasp-server",
                        "ws://localhost:8055",
                    ];
                    let mut p = CliTester::new_from_dir(&dir, args);
                    p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(())
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
            );
            cli_tester_handle.join().unwrap().expect("cli failed");

            get_announcement(&r53.events).clone()
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn identifier_from_coordinate(#[future] state_b_force: nostr::Event) -> Result<()> {
            let event = state_b_force.await;
            assert_eq!(
                get_tag_value(&event, "d"),
                "9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random"
            );
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn name_defaults_to_identifier(#[future] state_b_force: nostr::Event) -> Result<()> {
            let event = state_b_force.await;
            assert_eq!(
                get_tag_value(&event, "name"),
                "9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random"
            );
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn clone_url_from_grasp_server(#[future] state_b_force: nostr::Event) -> Result<()> {
            let event = state_b_force.await;
            let clone_urls = get_tag_values(&event, "clone");
            assert!(
                clone_urls
                    .iter()
                    .any(|u| u.starts_with("http://localhost:8055/")),
                "expected grasp-derived clone url, got: {:?}",
                clone_urls
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// State C: Existing announcement, it's mine
// ---------------------------------------------------------------------------

mod state_c_my_announcement {
    use futures::join;
    use test_utils::relay::Relay;

    use super::*;

    fn prep_git_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::default();
        test_repo.populate()?;
        test_repo.add_remote("origin", "https://localhost:1000")?;
        Ok(test_repo)
    }

    async fn run_init(extra_args: Vec<&str>) -> Result<nostr::Event> {
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
                            generate_repo_ref_event(),
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
                        &vec![
                            generate_test_key_1_metadata_event("fred"),
                            generate_test_key_1_relay_list_event(),
                            generate_repo_ref_event(),
                        ],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8056, None, None),
        );

        let cli_tester_handle = std::thread::spawn({
            let dir = git_repo.dir.clone();
            let extra_args_owned: Vec<String> = extra_args.iter().map(|s| s.to_string()).collect();
            move || -> Result<()> {
                let mut args = vec!["--nsec", TEST_KEY_1_NSEC, "--disable-cli-spinners", "init"];
                let extra_refs: Vec<&str> = extra_args_owned.iter().map(|s| s.as_str()).collect();
                args.extend(extra_refs);
                let mut p = CliTester::new_from_dir(&dir, args);
                p.expect_end_eventually()?;
                for port in [51, 52, 53, 55, 56] {
                    relay::shutdown_relay(8000 + port)?;
                }
                Ok(())
            }
        });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()?;

        Ok(get_announcement(&r53.events).clone())
    }

    mod errors {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn identifier_change_requires_force() -> Result<()> {
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
                                generate_repo_ref_event(),
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
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                                generate_repo_ref_event(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                move || -> Result<String> {
                    let args = vec![
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--disable-cli-spinners",
                        "init",
                        "--identifier",
                        "new-id",
                    ];
                    let mut p = CliTester::new_from_dir(&dir, args);
                    let output = p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(output)
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
            );
            let output = cli_tester_handle.join().unwrap()?;
            assert!(
                output.contains("changing identifier creates a new repository"),
                "expected identifier change error, got: {output}"
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn bare_no_flags_requires_force() -> Result<()> {
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
                                generate_repo_ref_event(),
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
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                                generate_repo_ref_event(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                move || -> Result<String> {
                    let args = vec!["--nsec", TEST_KEY_1_NSEC, "--disable-cli-spinners", "init"];
                    let mut p = CliTester::new_from_dir(&dir, args);
                    let output = p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(output)
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
            );
            let output = cli_tester_handle.join().unwrap()?;
            assert!(
                output.contains("no arguments specified"),
                "expected 'no arguments specified' error, got: {output}"
            );
            Ok(())
        }
    }

    mod success {
        use super::*;

        mod force_refresh {
            use super::*;

            #[fixture]
            async fn scenario() -> nostr::Event {
                run_init(vec!["--force"]).await.expect("init failed")
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn name_preserved(#[future] scenario: nostr::Event) -> Result<()> {
                let event = scenario.await;
                assert_eq!(get_tag_value(&event, "name"), "example name");
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn description_preserved(#[future] scenario: nostr::Event) -> Result<()> {
                let event = scenario.await;
                assert_eq!(get_tag_value(&event, "description"), "example description");
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn relays_from_my_event(#[future] scenario: nostr::Event) -> Result<()> {
                let event = scenario.await;
                let relays = get_tag_values(&event, "relays");
                assert!(
                    relays.contains(&"ws://localhost:8055".to_string()),
                    "relays should include my existing relay: {:?}",
                    relays
                );
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn maintainers_preserved(#[future] scenario: nostr::Event) -> Result<()> {
                let event = scenario.await;
                let maintainers = get_tag_values(&event, "maintainers");
                assert!(
                    maintainers.contains(&TEST_KEY_1_KEYS.public_key().to_string()),
                    "maintainers should include KEY_1: {:?}",
                    maintainers
                );
                assert!(
                    maintainers.contains(&TEST_KEY_2_KEYS.public_key().to_string()),
                    "maintainers should include KEY_2: {:?}",
                    maintainers
                );
                Ok(())
            }
        }

        mod name_override {
            use super::*;

            #[fixture]
            async fn scenario() -> nostr::Event {
                run_init(vec!["--name", "New Name"])
                    .await
                    .expect("init failed")
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn name_overridden(#[future] scenario: nostr::Event) -> Result<()> {
                let event = scenario.await;
                assert_eq!(get_tag_value(&event, "name"), "New Name");
                Ok(())
            }

            #[rstest]
            #[tokio::test]
            #[serial]
            async fn identifier_unchanged(#[future] scenario: nostr::Event) -> Result<()> {
                let event = scenario.await;
                assert_eq!(
                    get_tag_value(&event, "d"),
                    "9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random"
                );
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State D: Existing announcement, not mine, I'm listed as maintainer
// ---------------------------------------------------------------------------

mod state_d_co_maintainer {
    use futures::join;
    use test_utils::relay::Relay;

    use super::*;

    fn prep_git_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::without_repo_in_git_config();
        test_repo.populate()?;
        test_repo.add_remote("origin", "https://localhost:1000")?;
        test_repo.set_nostr_repo_coordinate(
            &TEST_KEY_2_KEYS.public_key(),
            "9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random",
            &["ws://localhost:8055", "ws://localhost:8056"],
        );
        Ok(test_repo)
    }

    mod success {
        use super::*;

        #[fixture]
        async fn scenario() -> nostr::Event {
            let git_repo = prep_git_repo().expect("prep failed");
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
                                generate_test_key_2_metadata_event("carole"),
                                generate_test_key_2_relay_list_event(),
                                generate_repo_ref_event_as_key_2_listing_key_1(),
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
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                                generate_test_key_2_metadata_event("carole"),
                                generate_test_key_2_relay_list_event(),
                                generate_repo_ref_event_as_key_2_listing_key_1(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                move || -> Result<()> {
                    let args = vec![
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--disable-cli-spinners",
                        "init",
                        "--grasp-server",
                        "ws://localhost:8055",
                    ];
                    let mut p = CliTester::new_from_dir(&dir, args);
                    p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(())
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
            );
            cli_tester_handle.join().unwrap().expect("cli failed");

            get_announcement(&r53.events).clone()
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn name_inherited_from_other_maintainer(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            assert_eq!(get_tag_value(&event, "name"), "example name");
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn description_inherited_from_other_maintainer(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            assert_eq!(get_tag_value(&event, "description"), "example description");
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn web_inherited_from_other_maintainer(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            let web = get_tag_values(&event, "web");
            assert!(
                web.iter().any(|w| w.contains("exampleproject.xyz")),
                "web should be inherited from KEY_2's announcement: {:?}",
                web
            );
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn clone_url_from_my_grasp_server_not_theirs(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            let clone_urls = get_tag_values(&event, "clone");
            assert!(
                clone_urls
                    .iter()
                    .any(|u| u.starts_with("http://localhost:8055/")),
                "clone url should be from my grasp server: {:?}",
                clone_urls
            );
            assert!(
                !clone_urls.iter().any(|u| u.contains("123.gitexample.com")),
                "clone url should NOT contain KEY_2's git server: {:?}",
                clone_urls
            );
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn relays_from_my_grasp_server(#[future] scenario: nostr::Event) -> Result<()> {
            let event = scenario.await;
            let relays = get_tag_values(&event, "relays");
            assert!(
                relays.contains(&"ws://localhost:8055".to_string()),
                "relays should include my grasp-derived relay: {:?}",
                relays
            );
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn maintainers_is_me_and_trusted(#[future] scenario: nostr::Event) -> Result<()> {
            let event = scenario.await;
            let maintainers = get_tag_values(&event, "maintainers");
            assert_eq!(
                maintainers.len(),
                2,
                "should have exactly 2 maintainers: {:?}",
                maintainers
            );
            assert!(
                maintainers.contains(&TEST_KEY_1_KEYS.public_key().to_string()),
                "maintainers should include KEY_1 (me): {:?}",
                maintainers
            );
            assert!(
                maintainers.contains(&TEST_KEY_2_KEYS.public_key().to_string()),
                "maintainers should include KEY_2 (trusted): {:?}",
                maintainers
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// State E: Existing announcement, not mine, I'm NOT listed as maintainer
// ---------------------------------------------------------------------------

mod state_e_not_listed {
    use futures::join;
    use test_utils::relay::Relay;

    use super::*;

    fn prep_git_repo() -> Result<GitTestRepo> {
        let test_repo = GitTestRepo::without_repo_in_git_config();
        test_repo.populate()?;
        test_repo.add_remote("origin", "https://localhost:1000")?;
        // Point coordinate to KEY_2 (not the logged-in user)
        test_repo.set_nostr_repo_coordinate(
            &TEST_KEY_2_KEYS.public_key(),
            "9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random",
            &["ws://localhost:8055", "ws://localhost:8056"],
        );
        Ok(test_repo)
    }

    /// Run init with relays that serve KEY_2's announcement NOT listing KEY_1.
    async fn run_init_expecting_error(extra_args: Vec<&str>) -> Result<String> {
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
                            generate_test_key_2_metadata_event("carole"),
                            generate_test_key_2_relay_list_event(),
                            generate_repo_ref_event_as_key_2_not_listing_key_1(),
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
                        &vec![
                            generate_test_key_1_metadata_event("fred"),
                            generate_test_key_1_relay_list_event(),
                            generate_test_key_2_metadata_event("carole"),
                            generate_test_key_2_relay_list_event(),
                            generate_repo_ref_event_as_key_2_not_listing_key_1(),
                        ],
                    )?;
                    Ok(())
                }),
            ),
            Relay::new(8056, None, None),
        );

        let cli_tester_handle = std::thread::spawn({
            let dir = git_repo.dir.clone();
            let extra_args_owned: Vec<String> = extra_args.iter().map(|s| s.to_string()).collect();
            move || -> Result<String> {
                let mut args = vec!["--nsec", TEST_KEY_1_NSEC, "--disable-cli-spinners", "init"];
                let extra_refs: Vec<&str> = extra_args_owned.iter().map(|s| s.as_str()).collect();
                args.extend(extra_refs);
                let mut p = CliTester::new_from_dir(&dir, args);
                let output = p.expect_end_eventually()?;
                for port in [51, 52, 53, 55, 56] {
                    relay::shutdown_relay(8000 + port)?;
                }
                Ok(output)
            }
        });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    mod errors {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn bare_no_flags() -> Result<()> {
            let output = run_init_expecting_error(vec![]).await?;
            assert!(
                output.contains("you are not listed as a maintainer"),
                "expected not-listed error, got: {output}"
            );
            Ok(())
        }

        #[tokio::test]
        #[serial]
        async fn defaults_still_requires_force() -> Result<()> {
            let output = run_init_expecting_error(vec!["--defaults"]).await?;
            assert!(
                output.contains("you are not listed as a maintainer"),
                "expected not-listed error even with -d, got: {output}"
            );
            Ok(())
        }
    }

    mod success {
        use super::*;

        #[fixture]
        async fn scenario() -> nostr::Event {
            let git_repo = prep_git_repo().expect("prep failed");
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
                                generate_test_key_2_metadata_event("carole"),
                                generate_test_key_2_relay_list_event(),
                                generate_repo_ref_event_as_key_2_not_listing_key_1(),
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
                            &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event(),
                                generate_test_key_2_metadata_event("carole"),
                                generate_test_key_2_relay_list_event(),
                                generate_repo_ref_event_as_key_2_not_listing_key_1(),
                            ],
                        )?;
                        Ok(())
                    }),
                ),
                Relay::new(8056, None, None),
            );

            let cli_tester_handle = std::thread::spawn({
                let dir = git_repo.dir.clone();
                move || -> Result<()> {
                    let args = vec![
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--disable-cli-spinners",
                        "init",
                        "--force",
                        "--grasp-server",
                        "ws://localhost:8055",
                    ];
                    let mut p = CliTester::new_from_dir(&dir, args);
                    p.expect_end_eventually()?;
                    for port in [51, 52, 53, 55, 56] {
                        relay::shutdown_relay(8000 + port)?;
                    }
                    Ok(())
                }
            });

            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
            );
            cli_tester_handle.join().unwrap().expect("cli failed");

            get_announcement(&r53.events).clone()
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn name_inherited_from_other_maintainer(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            assert_eq!(get_tag_value(&event, "name"), "example name");
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn description_inherited_from_other_maintainer(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            assert_eq!(get_tag_value(&event, "description"), "example description");
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn web_inherited_from_other_maintainer(
            #[future] scenario: nostr::Event,
        ) -> Result<()> {
            let event = scenario.await;
            let web = get_tag_values(&event, "web");
            assert!(
                web.iter().any(|w| w.contains("exampleproject.xyz")),
                "web should be inherited from KEY_2's announcement: {:?}",
                web
            );
            Ok(())
        }

        #[rstest]
        #[tokio::test]
        #[serial]
        async fn maintainers_is_me_and_trusted(#[future] scenario: nostr::Event) -> Result<()> {
            let event = scenario.await;
            let maintainers = get_tag_values(&event, "maintainers");
            assert_eq!(
                maintainers.len(),
                2,
                "should have exactly 2 maintainers: {:?}",
                maintainers
            );
            assert!(
                maintainers.contains(&TEST_KEY_1_KEYS.public_key().to_string()),
                "maintainers should include KEY_1 (me): {:?}",
                maintainers
            );
            assert!(
                maintainers.contains(&TEST_KEY_2_KEYS.public_key().to_string()),
                "maintainers should include KEY_2 (trusted): {:?}",
                maintainers
            );
            Ok(())
        }
    }
}
