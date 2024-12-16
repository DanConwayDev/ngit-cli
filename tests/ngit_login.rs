use anyhow::Result;
use git::GitTestRepo;
use serial_test::serial;
use test_utils::*;

static EXPECTED_NSEC_PROMPT: &str = "nsec";

fn show_first_time_login_choices(p: &mut CliTester) -> Result<CliTesterChoicePrompt> {
    p.expect_choice("login to nostr", vec![
        "secret key (nsec / ncryptsec)".to_string(),
        "nostr connect (remote signer)".to_string(),
        "create account".to_string(),
        "help".to_string(),
    ])
}

fn first_time_login_choices_succeeds_with_nsec(p: &mut CliTester, nsec: &str) -> Result<()> {
    p.expect_choice("login to nostr", vec![
        "secret key (nsec / ncryptsec)".to_string(),
        "nostr connect (remote signer)".to_string(),
        "create account".to_string(),
        "help".to_string(),
    ])?
    .succeeds_with(0, false, Some(0))?;

    p.expect_input(EXPECTED_NSEC_PROMPT)?
        .succeeds_with_optional_shortened_report(nsec, true)?;

    p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;
    Ok(())
}

fn standard_first_time_login_with_nsec() -> Result<CliTester> {
    let test_repo = GitTestRepo::default();
    let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login", "--offline"]);

    first_time_login_choices_succeeds_with_nsec(&mut p, TEST_KEY_1_NSEC)?;

    p.expect_end_eventually()?;
    Ok(p)
}

mod with_relays {
    use anyhow::Ok;
    use futures::join;
    use test_utils::relay::{ListenerReqFunc, Relay, shutdown_relay};

    use super::*;

    mod when_user_relay_list_aligns_with_fallback_relays {
        // this simplifies testing
        use super::*;

        mod when_first_time_login {
            use super::*;

            // falls_back_to_fallback_relays - this is implict in the tests

            mod dislays_logged_in_with_correct_name {

                use super::*;

                async fn run_test_displays_correct_name(
                    relay_listener1: Option<ListenerReqFunc<'_>>,
                    relay_listener2: Option<ListenerReqFunc<'_>>,
                ) -> Result<()> {
                    let (mut r51, mut r52) = (
                        Relay::new(8051, None, relay_listener1),
                        Relay::new(8052, None, relay_listener2),
                    );

                    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                        let test_repo = GitTestRepo::default();
                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login"]);

                        first_time_login_choices_succeeds_with_nsec(&mut p, TEST_KEY_1_NSEC)?;

                        p.expect("searching for profile...\r\n")?;

                        p.expect_end_with("logged in as fred\r\n")?;
                        for p in [51, 52] {
                            shutdown_relay(8000 + p)?;
                        }
                        Ok(())
                    });

                    // launch relay
                    let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                    cli_tester_handle.join().unwrap()?;
                    Ok(())
                }

                async fn run_test_displays_fallback_to_npub(
                    relay_listener1: Option<ListenerReqFunc<'_>>,
                    relay_listener2: Option<ListenerReqFunc<'_>>,
                ) -> Result<()> {
                    let (mut r51, mut r52) = (
                        Relay::new(8051, None, relay_listener1),
                        Relay::new(8052, None, relay_listener2),
                    );

                    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                        let test_repo = GitTestRepo::default();
                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login"]);

                        first_time_login_choices_succeeds_with_nsec(&mut p, TEST_KEY_1_NSEC)?;

                        p.expect("searching for profile...\r\n")?;

                        p.expect("failed to extract account name from account metadata...\r\n")?;

                        p.expect_end_with(
                            format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str(),
                        )?;
                        for p in [51, 52] {
                            shutdown_relay(8000 + p)?;
                        }
                        Ok(())
                    });

                    // launch relay
                    let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                    cli_tester_handle.join().unwrap()?;
                    Ok(())
                }

                #[tokio::test]
                #[serial]
                async fn when_latest_metadata_and_relay_list_on_all_relays() -> Result<()> {
                    run_test_displays_correct_name(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                    )
                    .await
                }

                mod poorly_quality_metadata_event {
                    use super::*;

                    #[tokio::test]
                    #[serial]
                    async fn when_metadata_contains_only_display_name() -> Result<()> {
                        run_test_displays_correct_name(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(client_id, &subscription_id, &vec![
                                    nostr::event::EventBuilder::metadata(
                                        &nostr::Metadata::new().display_name("fred"),
                                    )
                                    .sign_with_keys(&TEST_KEY_1_KEYS)
                                    .unwrap(),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ])?;
                                Ok(())
                            }),
                            None,
                        )
                        .await
                    }

                    #[tokio::test]
                    #[serial]
                    async fn when_metadata_contains_only_displayname() -> Result<()> {
                        println!(
                            "displayName: {}",
                            nostr::Metadata::new()
                                .custom_field("displayName", "fred")
                                .custom
                                .get("displayName")
                                .unwrap()
                        );
                        println!(
                            "name: {}",
                            nostr::Metadata::new().name("fred").name.unwrap()
                        );

                        run_test_displays_correct_name(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(client_id, &subscription_id, &vec![
                                    nostr::event::EventBuilder::metadata(
                                        &nostr::Metadata::new().custom_field("displayName", "fred"),
                                    )
                                    .sign_with_keys(&TEST_KEY_1_KEYS)
                                    .unwrap(),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ])?;
                                Ok(())
                            }),
                            None,
                        )
                        .await
                    }

                    #[tokio::test]
                    #[serial]
                    async fn displays_npub_when_metadata_contains_no_name_displayname_or_display_name()
                    -> Result<()> {
                        run_test_displays_fallback_to_npub(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(client_id, &subscription_id, &vec![
                                    nostr::event::EventBuilder::metadata(
                                        &nostr::Metadata::new().about("other info in metadata"),
                                    )
                                    .sign_with_keys(&TEST_KEY_1_KEYS)
                                    .unwrap(),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ])?;
                                Ok(())
                            }),
                            None,
                        )
                        .await
                    }
                }

                #[tokio::test]
                #[serial]
                async fn when_latest_metadata_and_relay_list_on_some_relays_but_others_have_none()
                -> Result<()> {
                    run_test_displays_correct_name(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                        None,
                    )
                    .await
                }

                #[tokio::test]
                #[serial]
                async fn when_latest_metadata_only_on_relay_and_relay_list_on_another() -> Result<()>
                {
                    run_test_displays_correct_name(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                            ])?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                    )
                    .await
                }

                #[tokio::test]
                #[serial]
                async fn when_some_relays_return_old_metadata_event() -> Result<()> {
                    run_test_displays_correct_name(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event_old("fred old"),
                            ])?;
                            Ok(())
                        }),
                    )
                    .await
                }

                #[tokio::test]
                #[serial]
                async fn when_some_relays_return_other_users_metadata() -> Result<()> {
                    run_test_displays_correct_name(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_2_metadata_event("carole"),
                            ])?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event_old("fred"),
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                    )
                    .await
                }

                #[tokio::test]
                #[serial]
                async fn when_some_relays_return_other_event_kinds() -> Result<()> {
                    run_test_displays_correct_name(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            let event = generate_test_key_1_kind_event(nostr::Kind::TextNote);
                            relay.respond_events(client_id, &subscription_id, &vec![
                                make_event_old_or_change_user(event, &TEST_KEY_1_KEYS, 0),
                            ])?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event_old("fred"),
                                generate_test_key_1_relay_list_event_same_as_fallback(),
                            ])?;
                            Ok(())
                        }),
                    )
                    .await
                }

                mod when_specifying_command_line_nsec {
                    use super::*;

                    #[tokio::test]
                    #[serial]
                    async fn displays_correct_name() -> Result<()> {
                        run_test_when_specifying_command_line_nsec_only_displays_correct_name(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(client_id, &subscription_id, &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ])?;
                                Ok(())
                            }),
                            None,
                        )
                        .await
                    }
                    async fn run_test_when_specifying_command_line_nsec_only_displays_correct_name(
                        relay_listener1: Option<ListenerReqFunc<'_>>,
                        relay_listener2: Option<ListenerReqFunc<'_>>,
                    ) -> Result<()> {
                        let (mut r51, mut r52) = (
                            Relay::new(8051, None, relay_listener1),
                            Relay::new(8052, None, relay_listener2),
                        );

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            let test_repo = GitTestRepo::default();
                            let mut p = CliTester::new_from_dir(&test_repo.dir, [
                                "account",
                                "login",
                                "--nsec",
                                TEST_KEY_1_NSEC,
                            ]);

                            p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

                            p.expect("searching for profile...\r\n")?;

                            p.expect_end_with("logged in as fred via cli arguments\r\n")?;
                            for p in [51, 52] {
                                shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        });

                        // launch relay
                        let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                        cli_tester_handle.join().unwrap()?;
                        Ok(())
                    }
                }
            }
            mod when_no_metadata_found {
                use super::*;

                #[tokio::test]
                #[serial]
                async fn warm_user_and_displays_npub() -> Result<()> {
                    run_test_when_no_metadata_found_warns_user_and_uses_npub(None, None).await
                }

                async fn run_test_when_no_metadata_found_warns_user_and_uses_npub(
                    relay_listener1: Option<ListenerReqFunc<'_>>,
                    relay_listener2: Option<ListenerReqFunc<'_>>,
                ) -> Result<()> {
                    let (mut r51, mut r52) = (
                        Relay::new(8051, None, relay_listener1),
                        Relay::new(8052, None, relay_listener2),
                    );

                    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                        let test_repo = GitTestRepo::default();
                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login"]);

                        first_time_login_choices_succeeds_with_nsec(&mut p, TEST_KEY_1_NSEC)?;

                        p.expect("searching for profile...\r\n")?;

                        p.expect("failed to find profile...\r\n")?;

                        p.expect_end_with(format!("logged in as {TEST_KEY_1_NPUB}\r\n").as_str())?;
                        for p in [51, 52] {
                            shutdown_relay(8000 + p)?;
                        }
                        Ok(())
                    });

                    // launch relay
                    let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                    cli_tester_handle.join().unwrap()?;
                    Ok(())
                }
            }

            mod when_metadata_but_no_relay_list_found {
                use super::*;

                #[tokio::test]
                #[serial]
                async fn warm_user_and_displays_name() -> Result<()> {
                    run_test_when_no_relay_list_found_warns_user_and_uses_npub(
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(client_id, &subscription_id, &vec![
                                generate_test_key_1_metadata_event("fred"),
                            ])?;
                            Ok(())
                        }),
                        None,
                    )
                    .await
                }

                async fn run_test_when_no_relay_list_found_warns_user_and_uses_npub(
                    relay_listener1: Option<ListenerReqFunc<'_>>,
                    relay_listener2: Option<ListenerReqFunc<'_>>,
                ) -> Result<()> {
                    let (mut r51, mut r52) = (
                        Relay::new(8051, None, relay_listener1),
                        Relay::new(8052, None, relay_listener2),
                    );

                    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                        let test_repo = GitTestRepo::default();
                        let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login"]);

                        first_time_login_choices_succeeds_with_nsec(&mut p, TEST_KEY_1_NSEC)?;

                        p.expect("searching for profile...\r\n")?;

                        p.expect("failed to find your relay list. consider using another nostr client to create one to enhance your nostr experience.\r\n")?;

                        p.expect_end_with("logged in as fred\r\n")?;
                        for p in [51, 52] {
                            shutdown_relay(8000 + p)?;
                        }
                        Ok(())
                    });

                    // launch relay
                    let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                    cli_tester_handle.join().unwrap()?;
                    Ok(())
                }
            }
        }
    }
    mod when_user_relay_list_contains_write_relays_not_in_fallback_list {
        use super::*;
        mod when_latest_metadata_not_on_fallback_relays_only_on_relays_in_user_list {
            use super::*;
            async fn run_test_displays_correct_name(
                relay_listener1: Option<ListenerReqFunc<'_>>,
                relay_listener2: Option<ListenerReqFunc<'_>>,
            ) -> Result<()> {
                let (mut r51, mut r52, mut r53, mut r55) = (
                    Relay::new(8051, None, relay_listener1),
                    Relay::new(8052, None, None),
                    Relay::new(8053, None, relay_listener2),
                    Relay::new(8055, None, None),
                );

                let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                    let test_repo = GitTestRepo::default();
                    let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login"]);

                    first_time_login_choices_succeeds_with_nsec(&mut p, TEST_KEY_1_NSEC)?;

                    p.expect("searching for profile...\r\n")?;

                    p.expect_end_with("logged in as fred\r\n")?;
                    for p in [51, 52, 53, 55] {
                        shutdown_relay(8000 + p)?;
                    }
                    Ok(())
                });

                // launch relay
                let _ = join!(
                    r51.listen_until_close(),
                    r52.listen_until_close(),
                    r53.listen_until_close(),
                    r55.listen_until_close(),
                );

                cli_tester_handle.join().unwrap()?;
                Ok(())
            }

            /// this also tests that additional relays are queried
            #[tokio::test]
            #[serial]
            async fn displays_correct_name() -> Result<()> {
                run_test_displays_correct_name(
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(client_id, &subscription_id, &vec![
                            generate_test_key_1_metadata_event_old("Fred"),
                            generate_test_key_1_relay_list_event(),
                        ])?;
                        Ok(())
                    }),
                    Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                        relay.respond_events(client_id, &subscription_id, &vec![
                            generate_test_key_1_metadata_event("fred"),
                            generate_test_key_1_relay_list_event(),
                        ])?;
                        Ok(())
                    }),
                )
                .await
            }
        }
    }
}

/// using the offline flag simplifies the test. relay interaction is tested
/// seperately
mod with_offline_flag {
    use super::*;
    mod when_first_time_login {
        use super::*;

        #[test]
        fn prompts_for_nsec_and_password() -> Result<()> {
            standard_first_time_login_with_nsec()?;
            Ok(())
        }

        #[test]
        fn succeeds_with_text_logged_in_as_npub() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login", "--offline"]);

            show_first_time_login_choices(&mut p)?.succeeds_with(0, false, Some(0))?;

            p.expect_input(EXPECTED_NSEC_PROMPT)?
                .succeeds_with_optional_shortened_report(TEST_KEY_1_NSEC, true)?;

            p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

            p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
        }

        #[test]
        fn succeeds_with_hex_secret_key_in_place_of_nsec() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let mut p = CliTester::new_from_dir(&test_repo.dir, ["account", "login", "--offline"]);

            show_first_time_login_choices(&mut p)?.succeeds_with(0, false, Some(0))?;

            p.expect_input(EXPECTED_NSEC_PROMPT)?
                .succeeds_with_optional_shortened_report(TEST_KEY_1_SK_HEX, true)?;

            p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

            p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
        }

        mod when_invalid_nsec {
            use super::*;

            #[test]
            fn prompts_for_nsec_until_valid() -> Result<()> {
                let test_repo = GitTestRepo::default();
                let mut p =
                    CliTester::new_from_dir(&test_repo.dir, ["account", "login", "--offline"]);

                show_first_time_login_choices(&mut p)?.succeeds_with(0, false, Some(0))?;

                for _ in 0..2 {
                    p.expect_input(EXPECTED_NSEC_PROMPT)?
                        .fails_with_optional_shortened_report(
                            TEST_INVALID_NSEC,
                            Some("invalid "),
                            true,
                        )?;

                    p.expect_choice("login to nostr", vec![
                        "try again with nsec".to_string(),
                        "back".to_string(),
                    ])?
                    .succeeds_with(0, false, Some(0))?;
                }

                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    .succeeds_with_optional_shortened_report(TEST_KEY_1_NSEC, true)?;

                p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            }
        }
    }

    mod when_called_with_nsec_parameter_only {
        use super::*;

        #[test]
        fn valid_nsec_param_succeeds_without_prompts() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let mut p = CliTester::new_from_dir(&test_repo.dir, [
                "account",
                "login",
                "--offline",
                "--nsec",
                TEST_KEY_1_NSEC,
            ]);

            p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

            p.expect_end_with(
                format!("logged in as {} via cli arguments\r\n", TEST_KEY_1_NPUB).as_str(),
            )
        }

        #[test]
        fn invalid_nsec_param_fails_without_prompts() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let mut p = CliTester::new_from_dir(&test_repo.dir, [
                "account",
                "login",
                "--offline",
                "--nsec",
                TEST_INVALID_NSEC,
            ]);

            p.expect_end_with(
                "Error: invalid nsec parameter\r\n\r\nCaused by:\r\n    Invalid secret key\r\n",
            )
        }
    }

    mod when_called_with_nsec_and_password_parameter {
        use super::*;

        #[test]
        fn valid_nsec_param_succeeds_without_prompts() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let mut p = CliTester::new_from_dir(&test_repo.dir, [
                "account",
                "login",
                "--offline",
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
            ]);
            p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

            p.expect_end_with(
                format!("logged in as {} via cli arguments\r\n", TEST_KEY_1_NPUB).as_str(),
            )
        }

        #[test]
        fn parameters_can_be_called_globally() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let mut p = CliTester::new_from_dir(&test_repo.dir, [
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
                "account",
                "login",
                "--offline",
            ]);
            p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

            p.expect_end_with(
                format!("logged in as {} via cli arguments\r\n", TEST_KEY_1_NPUB).as_str(),
            )
        }

        mod when_logging_in_as_different_nsec {
            use super::*;

            #[test]
            fn valid_nsec_param_succeeds_without_prompts_and_logs_in() -> Result<()> {
                standard_first_time_login_with_nsec()?.exit()?;
                let test_repo = GitTestRepo::default();
                let mut p = CliTester::new_from_dir(&test_repo.dir, [
                    "account",
                    "login",
                    "--offline",
                    "--nsec",
                    TEST_KEY_2_NSEC,
                    "--password",
                    TEST_PASSWORD,
                ]);
                p.expect("saved login details to local git config. you are only logged in to this local repository.\r\n")?;

                p.expect_end_with(
                    format!("logged in as {} via cli arguments\r\n", TEST_KEY_2_NPUB).as_str(),
                )
            }
        }
    }
}
