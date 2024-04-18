use anyhow::Result;
use serial_test::serial;
use test_utils::*;

static EXPECTED_NSEC_PROMPT: &str = "login with nsec (or hex private key)";
static EXPECTED_SET_PASSWORD_PROMPT: &str = "encrypt with password";
static EXPECTED_SET_PASSWORD_CONFIRM_PROMPT: &str = "confirm password";
static EXPECTED_PASSWORD_PROMPT: &str = "password";

fn standard_login() -> Result<CliTester> {
    let mut p = CliTester::new(["login", "--offline"]);

    p.expect_input_eventually(EXPECTED_NSEC_PROMPT)?
        .succeeds_with(TEST_KEY_1_NSEC)?;

    p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
        .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
        .succeeds_with(TEST_PASSWORD)?;

    p.expect_end_eventually()?;
    Ok(p)
}
mod with_relays {
    use anyhow::Ok;
    use futures::join;
    use test_utils::relay::{shutdown_relay, ListenerReqFunc, Relay};

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
                        with_fresh_config(|| {
                            let mut p = CliTester::new(["login"]);

                            p.expect_input(EXPECTED_NSEC_PROMPT)?
                                .succeeds_with(TEST_KEY_1_NSEC)?;

                            p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                                .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                                .succeeds_with(TEST_PASSWORD)?;

                            p.expect("searching for profile and relay updates...\r\n")?;
                            p.expect("\r")?;

                            // p.expect_end_with(
                            //     format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str(),
                            // )?;
                            p.expect_end_with("logged in as fred\r\n")?;
                            for p in [51, 52] {
                                shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        })
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
                        with_fresh_config(|| {
                            let mut p = CliTester::new(["login"]);

                            p.expect_input(EXPECTED_NSEC_PROMPT)?
                                .succeeds_with(TEST_KEY_1_NSEC)?;

                            p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                                .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                                .succeeds_with(TEST_PASSWORD)?;

                            p.expect("searching for profile and relay updates...\r\n")?;
                            p.expect("\r")?;

                            p.expect_end_with(
                                format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str(),
                            )?;
                            for p in [51, 52] {
                                shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        })
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
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
                                relay.respond_events(
                                    client_id,
                                    &subscription_id,
                                    &vec![
                                        nostr::event::EventBuilder::metadata(
                                            &nostr::Metadata::new().display_name("fred"),
                                        )
                                        .to_event(&TEST_KEY_1_KEYS)
                                        .unwrap(),
                                        generate_test_key_1_relay_list_event_same_as_fallback(),
                                    ],
                                )?;
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
                                relay.respond_events(
                                    client_id,
                                    &subscription_id,
                                    &vec![
                                        nostr::event::EventBuilder::metadata(
                                            &nostr::Metadata::new()
                                                .custom_field("displayName", "fred"),
                                        )
                                        .to_event(&TEST_KEY_1_KEYS)
                                        .unwrap(),
                                        generate_test_key_1_relay_list_event_same_as_fallback(),
                                    ],
                                )?;
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

                        run_test_displays_fallback_to_npub(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(
                                    client_id,
                                    &subscription_id,
                                    &vec![
                                        nostr::event::EventBuilder::metadata(
                                            &nostr::Metadata::new(),
                                        )
                                        .to_event(&TEST_KEY_1_KEYS)
                                        .unwrap(),
                                        generate_test_key_1_relay_list_event_same_as_fallback(),
                                    ],
                                )?;
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_test_key_1_metadata_event("fred")],
                            )?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_test_key_1_relay_list_event_same_as_fallback()],
                            )?;
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_test_key_1_metadata_event_old("fred old")],
                            )?;
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_test_key_2_metadata_event("carole")],
                            )?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event_old("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![make_event_old_or_change_user(event, &TEST_KEY_1_KEYS, 0)],
                            )?;
                            Ok(())
                        }),
                        Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event_old("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
                            Ok(())
                        }),
                    )
                    .await
                }

                mod when_specifying_command_line_nsec_only {
                    use super::*;

                    #[tokio::test]
                    #[serial]
                    async fn displays_correct_name() -> Result<()> {
                        run_test_when_specifying_command_line_nsec_only_displays_correct_name(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(
                                    client_id,
                                    &subscription_id,
                                    &vec![
                                        generate_test_key_1_metadata_event("fred"),
                                        generate_test_key_1_relay_list_event_same_as_fallback(),
                                    ],
                                )?;
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
                            with_fresh_config(|| {
                                let mut p = CliTester::new(["login", "--nsec", TEST_KEY_1_NSEC]);

                                p.expect("searching for profile and relay updates...\r\n")?;
                                p.expect("\r")?;

                                p.expect_end_with("logged in as fred\r\n")?;
                                for p in [51, 52] {
                                    shutdown_relay(8000 + p)?;
                                }
                                Ok(())
                            })
                        });

                        // launch relay
                        let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                        cli_tester_handle.join().unwrap()?;
                        Ok(())
                    }
                }
                mod when_specifying_command_line_password_only {
                    use super::*;

                    #[tokio::test]
                    #[serial]
                    async fn displays_correct_name() -> Result<()> {
                        run_test_when_specifying_command_line_password_only_displays_correct_name(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(
                                    client_id,
                                    &subscription_id,
                                    &vec![
                                        generate_test_key_1_metadata_event("fred"),
                                        generate_test_key_1_relay_list_event_same_as_fallback(),
                                    ],
                                )?;
                                Ok(())
                            }),
                            None,
                        )
                        .await
                    }
                    async fn run_test_when_specifying_command_line_password_only_displays_correct_name(
                        relay_listener1: Option<ListenerReqFunc<'_>>,
                        relay_listener2: Option<ListenerReqFunc<'_>>,
                    ) -> Result<()> {
                        let (mut r51, mut r52) = (
                            Relay::new(8051, None, relay_listener1),
                            Relay::new(8052, None, relay_listener2),
                        );

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            with_fresh_config(|| {
                                CliTester::new([
                                    "login",
                                    "--offline",
                                    "--nsec",
                                    TEST_KEY_1_NSEC,
                                    "--password",
                                    TEST_PASSWORD,
                                ])
                                .expect_end_eventually()?;

                                let mut p = CliTester::new(["login", "--password", TEST_PASSWORD]);

                                p.expect("searching for profile and relay updates...\r\n")?;
                                p.expect("\r")?;

                                p.expect_end_with("logged in as fred\r\n")?;
                                for p in [51, 52] {
                                    shutdown_relay(8000 + p)?;
                                }
                                Ok(())
                            })
                        });

                        // launch relay
                        let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                        cli_tester_handle.join().unwrap()?;
                        Ok(())
                    }
                }

                mod when_specifying_command_line_nsec_and_password {
                    use super::*;

                    #[tokio::test]
                    #[serial]
                    async fn displays_correct_name() -> Result<()> {
                        run_test_when_specifying_command_line_nsec_and_password_displays_correct_name(
                            Some(&|relay, client_id, subscription_id, _| -> Result<()> {
                                relay.respond_events(
                                    client_id,
                                    &subscription_id,
                                    &vec![
                                        generate_test_key_1_metadata_event("fred"),
                                        generate_test_key_1_relay_list_event_same_as_fallback(),
                                    ],
                                )?;
                                Ok(())
                            }),
                            None,
                        ).await
                    }
                    async fn run_test_when_specifying_command_line_nsec_and_password_displays_correct_name(
                        relay_listener1: Option<ListenerReqFunc<'_>>,
                        relay_listener2: Option<ListenerReqFunc<'_>>,
                    ) -> Result<()> {
                        let (mut r51, mut r52) = (
                            Relay::new(8051, None, relay_listener1),
                            Relay::new(8052, None, relay_listener2),
                        );

                        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                            with_fresh_config(|| {
                                let mut p = CliTester::new([
                                    "login",
                                    "--nsec",
                                    TEST_KEY_1_NSEC,
                                    "--password",
                                    TEST_PASSWORD,
                                ]);

                                p.expect("searching for profile and relay updates...\r\n")?;
                                p.expect("\r")?;

                                p.expect_end_with("logged in as fred\r\n")?;
                                for p in [51, 52] {
                                    shutdown_relay(8000 + p)?;
                                }
                                Ok(())
                            })
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
                        with_fresh_config(|| {
                            let mut p = CliTester::new(["login"]);

                            p.expect_input(EXPECTED_NSEC_PROMPT)?
                                .succeeds_with(TEST_KEY_1_NSEC)?;

                            p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                                .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                                .succeeds_with(TEST_PASSWORD)?;

                            p.expect("searching for profile and relay updates...\r\n")?;
                            p.expect("\r")?;
                            p.expect(
                                "cannot find your account metadata (name, etc) on relays\r\n",
                            )?;

                            p.expect_end_with(
                                format!("logged in as {TEST_KEY_1_NPUB}\r\n").as_str(),
                            )?;
                            for p in [51, 52] {
                                shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        })
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
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![generate_test_key_1_metadata_event("fred")],
                            )?;
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
                        with_fresh_config(|| {
                            let mut p = CliTester::new(["login"]);

                            p.expect_input(EXPECTED_NSEC_PROMPT)?
                                .succeeds_with(TEST_KEY_1_NSEC)?;

                            p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                                .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                                .succeeds_with(TEST_PASSWORD)?;

                            p.expect("searching for profile and relay updates...\r\n")?;
                            p.expect("\r")?;
                            p.expect("cannot find your relay list. consider using another nostr client to create one to enhance your nostr experience.\r\n")?;

                            p.expect_end_with("logged in as fred\r\n")?;
                            for p in [51, 52] {
                                shutdown_relay(8000 + p)?;
                            }
                            Ok(())
                        })
                    });

                    // launch relay
                    let _ = join!(r51.listen_until_close(), r52.listen_until_close(),);

                    cli_tester_handle.join().unwrap()?;
                    Ok(())
                }
            }
        }

        mod when_second_time_login_and_details_already_fetched {
            use super::*;

            // TODO: the following two tests would require a fake config file or
            // fake time
            //  - uses_relays_from_user_relay_list
            //  - dislays_correct_name - when_local_metadata_is_the_most_recent

            mod uses_cache {
                use super::*;

                #[tokio::test]
                #[serial]
                async fn dislays_logged_in_with_correct_name() -> Result<()> {
                    run_test_dislays_logged_in_with_correct_name(Some(
                        &|relay, client_id, subscription_id, _| -> Result<()> {
                            relay.respond_events(
                                client_id,
                                &subscription_id,
                                &vec![
                                    generate_test_key_1_metadata_event("fred"),
                                    generate_test_key_1_relay_list_event_same_as_fallback(),
                                ],
                            )?;
                            Ok(())
                        },
                    ))
                    .await
                }
                async fn run_test_dislays_logged_in_with_correct_name(
                    relay_listener: Option<ListenerReqFunc<'_>>,
                ) -> Result<()> {
                    let (mut r51, mut r52) = (
                        Relay::new(8051, None, relay_listener),
                        Relay::new(8052, None, None),
                    );

                    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                        with_fresh_config(|| {
                            let mut p = CliTester::new([
                                "login",
                                "--nsec",
                                TEST_KEY_1_NSEC,
                                "--password",
                                TEST_PASSWORD,
                            ]);

                            p.expect_end_eventually_with("logged in as fred\r\n")?;

                            for p in [51, 52] {
                                shutdown_relay(8000 + p)?;
                            }

                            let mut p = CliTester::new(["login", "--password", TEST_PASSWORD]);

                            p.expect("searching for profile and relay updates...\r\n")?;
                            p.expect("\r")?;

                            p.expect_end_eventually_with("logged in as fred\r\n")?;

                            Ok(())
                        })
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
                    with_fresh_config(|| {
                        let mut p = CliTester::new(["login"]);

                        p.expect_input(EXPECTED_NSEC_PROMPT)?
                            .succeeds_with(TEST_KEY_1_NSEC)?;

                        p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                            .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                            .succeeds_with(TEST_PASSWORD)?;

                        p.expect("searching for profile and relay updates...\r\n")?;
                        p.expect("\r")?;

                        p.expect_end_with("logged in as fred\r\n")?;
                        for p in [51, 52, 53, 55] {
                            shutdown_relay(8000 + p)?;
                        }
                        Ok(())
                    })
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
                        relay.respond_events(
                            client_id,
                            &subscription_id,
                            &vec![
                                generate_test_key_1_metadata_event_old("Fred"),
                                generate_test_key_1_relay_list_event(),
                            ],
                        )?;
                        Ok(())
                    }),
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
        #[serial]
        fn prompts_for_nsec_and_password() -> Result<()> {
            before()?;
            standard_login()?;
            after()
        }

        #[test]
        #[serial]
        fn succeeds_with_text_logged_in_as_npub() -> Result<()> {
            with_fresh_config(|| {
                let mut p = CliTester::new(["login", "--offline"]);

                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    .succeeds_with(TEST_KEY_1_NSEC)?;

                p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                    .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                    .succeeds_with(TEST_PASSWORD)?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        #[test]
        #[serial]
        fn succeeds_with_hex_secret_key_in_place_of_nsec() -> Result<()> {
            with_fresh_config(|| {
                let mut p = CliTester::new(["login", "--offline"]);

                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    .succeeds_with(TEST_KEY_1_SK_HEX)?;

                p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                    .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                    .succeeds_with(TEST_PASSWORD)?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        mod when_invalid_nsec {
            use super::*;

            #[test]
            #[serial]
            fn prompts_for_nsec_until_valid() -> Result<()> {
                with_fresh_config(|| {
                    let invalid_nsec_response =
                        "invalid nsec. try again with nsec (or hex private key)";

                    let mut p = CliTester::new(["login", "--offline"]);

                    p.expect_input(EXPECTED_NSEC_PROMPT)?
                        // this behaviour is intentional. rejecting the response with dialoguer
                        // hides the original input from the user so they
                        // cannot see the mistake they made.
                        .succeeds_with(TEST_INVALID_NSEC)?;

                    p.expect_input(invalid_nsec_response)?
                        .succeeds_with(TEST_INVALID_NSEC)?;

                    p.expect_input(invalid_nsec_response)?
                        .succeeds_with(TEST_KEY_1_NSEC)?;

                    p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                        .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                        .succeeds_with(TEST_PASSWORD)?;

                    p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
                })
            }
        }
    }

    mod when_second_time_login {
        use super::*;

        #[test]
        #[serial]
        fn prints_login_as_npub() -> Result<()> {
            with_fresh_config(|| {
                standard_login()?.exit()?;

                CliTester::new(["login", "--offline"])
                    .expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                    .exit()
            })
        }

        #[test]
        #[serial]
        fn prompts_for_password_and_succeeds_with_logged_in_as_npub() -> Result<()> {
            with_fresh_config(|| {
                standard_login()?.exit()?;

                let mut p = CliTester::new(["login", "--offline"]);

                p.expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                    .expect_password(EXPECTED_PASSWORD_PROMPT)?
                    .succeeds_with(TEST_PASSWORD)?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        #[test]
        #[serial]
        fn when_invalid_password_exit_with_error() -> Result<()> {
            with_fresh_config(|| {
                standard_login()?.exit()?;

                let mut p = CliTester::new(["login", "--offline"]);

                p.expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                    .expect_password(EXPECTED_PASSWORD_PROMPT)?
                    .succeeds_with(TEST_INVALID_PASSWORD)?;
                p.expect_end_with(format!("Error: failed to log in as {}\r\n\r\nCaused by:\r\n    0: failed to decrypt key with provided password\r\n    1: ChaCha20Poly1305: aead::Error\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }
    }

    mod when_called_with_nsec_parameter_only {
        use super::*;

        #[test]
        #[serial]
        fn valid_nsec_param_succeeds_without_prompts() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new(["login", "--offline", "--nsec", TEST_KEY_1_NSEC])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        #[test]
        #[serial]
        fn forgets_identity() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new(["login", "--offline", "--nsec", TEST_KEY_1_NSEC])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

                let mut p = CliTester::new(["login", "--offline"]);

                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    .succeeds_with(TEST_KEY_1_NSEC)?;

                p.exit()
            })
        }

        mod when_logging_in_as_different_nsec {
            use super::*;

            #[test]
            #[serial]
            fn valid_nsec_param_succeeds_without_prompts_and_logs_in() -> Result<()> {
                with_fresh_config(|| {
                    standard_login()?.exit()?;

                    CliTester::new(["login", "--offline", "--nsec", TEST_KEY_2_NSEC])
                        .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NPUB).as_str())
                })
            }
        }
        #[test]
        #[serial]
        fn invalid_nsec_param_fails_without_prompts() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new(["login", "--offline", "--nsec", TEST_INVALID_NSEC]).expect_end_with(
                    "Error: invalid nsec parameter\r\n\r\nCaused by:\r\n    Invalid secret key\r\n",
                )
            })
        }
    }

    mod when_called_with_nsec_and_password_parameter {
        use super::*;

        #[test]
        #[serial]
        fn valid_nsec_param_succeeds_without_prompts() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new([
                    "login",
                    "--offline",
                    "--nsec",
                    TEST_KEY_1_NSEC,
                    "--password",
                    TEST_PASSWORD,
                ])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        #[test]
        #[serial]
        fn remembers_identity() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new([
                    "login",
                    "--offline",
                    "--nsec",
                    TEST_KEY_1_NSEC,
                    "--password",
                    TEST_PASSWORD,
                ])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

                CliTester::new(["login", "--offline"])
                    .expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                    .exit()
            })
        }

        #[test]
        #[serial]
        fn parameters_can_be_called_globally() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new([
                    "--nsec",
                    TEST_KEY_1_NSEC,
                    "--password",
                    TEST_PASSWORD,
                    "login",
                    "--offline",
                ])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        mod when_logging_in_as_different_nsec {
            use super::*;

            #[test]
            #[serial]
            fn valid_nsec_param_succeeds_without_prompts_and_logs_in() -> Result<()> {
                with_fresh_config(|| {
                    standard_login()?.exit()?;

                    CliTester::new([
                        "login",
                        "--offline",
                        "--nsec",
                        TEST_KEY_2_NSEC,
                        "--password",
                        TEST_PASSWORD,
                    ])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NPUB).as_str())
                })
            }

            #[test]
            #[serial]
            fn remembers_identity() -> Result<()> {
                with_fresh_config(|| {
                    standard_login()?.exit()?;

                    CliTester::new([
                        "login",
                        "--offline",
                        "--nsec",
                        TEST_KEY_2_NSEC,
                        "--password",
                        TEST_PASSWORD,
                    ])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NPUB).as_str())?;

                    CliTester::new(["login", "--offline"])
                        .expect(format!("login as {}\r\n", TEST_KEY_2_NPUB).as_str())?
                        .exit()
                })
            }
        }

        mod when_provided_with_new_password {
            use super::*;

            #[test]
            #[serial]
            fn password_changes() -> Result<()> {
                with_fresh_config(|| {
                    standard_login()?.exit()?;

                    CliTester::new([
                        "login",
                        "--offline",
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--password",
                        TEST_INVALID_PASSWORD,
                    ])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

                    CliTester::new(["--password", TEST_INVALID_PASSWORD, "login", "--offline"])
                        .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
                })
            }
        }

        #[test]
        #[serial]
        fn invalid_nsec_param_fails_without_prompts() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new([
                    "login",
                    "--offline",
                    "--nsec",
                    TEST_INVALID_NSEC,
                    "--password",
                    TEST_PASSWORD,
                ])
                .expect_end_with(
                    "Error: invalid nsec parameter\r\n\r\nCaused by:\r\n    Invalid secret key\r\n",
                )
            })
        }
    }

    mod when_called_with_password_parameter_only {
        use super::*;

        #[test]
        #[serial]
        fn when_nsec_stored_logs_in_without_prompts() -> Result<()> {
            with_fresh_config(|| {
                standard_login()?.exit()?;

                CliTester::new(["login", "--offline", "--password", TEST_PASSWORD])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }

        #[test]
        #[serial]
        fn when_no_nsec_stored_logs_error() -> Result<()> {
            with_fresh_config(|| {
                CliTester::new(["login", "--offline", "--password", TEST_PASSWORD]).expect_end_with(
                    "Error: no nsec available to decrypt with specified password\r\n",
                )
            })
        }
    }

    mod when_weak_password {
        use super::*;

        #[test]
        #[serial]
        // combined into a single test as it is computationally expensive to run
        fn warns_it_might_take_a_few_seconds_then_succeeds_then_second_login_prompts_for_password_then_warns_again_then_succeeds()
        -> Result<()> {
            with_fresh_config(|| {
                let mut p = CliTester::new_with_timeout(10000, ["login", "--offline"]);
                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    .succeeds_with(TEST_KEY_1_NSEC)?;

                p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                    .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                    .succeeds_with(TEST_WEAK_PASSWORD)?;

                p.expect("this may take a few seconds...\r\n")?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

                p = CliTester::new_with_timeout(10000, ["login", "--offline"]);

                p.expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                    .expect_password(EXPECTED_PASSWORD_PROMPT)?
                    .succeeds_with(TEST_WEAK_PASSWORD)?;

                p.expect("this may take a few seconds...\r\n")?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
            })
        }
    }
}
