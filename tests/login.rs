use anyhow::Result;
use serial_test::serial;
use test_utils::*;

static EXPECTED_NSEC_PROMPT: &str = "login with nsec (or hex private key)";
static EXPECTED_SET_PASSWORD_PROMPT: &str = "encrypt with password";
static EXPECTED_SET_PASSWORD_CONFIRM_PROMPT: &str = "confirm password";
static EXPECTED_PASSWORD_PROMPT: &str = "password";

fn standard_login() -> Result<CliTester> {
    let mut p = CliTester::new(["login"]);

    p.expect_input_eventually(EXPECTED_NSEC_PROMPT)?
        .succeeds_with(TEST_KEY_1_NSEC)?;

    p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
        .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
        .succeeds_with(TEST_PASSWORD)?;

    p.expect_end_eventually()?;
    Ok(p)
}

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
            let mut p = CliTester::new(["login"]);

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
            let mut p = CliTester::new(["login"]);

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

                let mut p = CliTester::new(["login"]);

                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    // this behaviour is intentional. rejecting the response with dialoguer hides
                    // the original input from the user so they cannot see the
                    // mistake they made.
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

            CliTester::new(["login"])
                .expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                .exit()
        })
    }

    #[test]
    #[serial]
    fn prompts_for_password_and_succeeds_with_logged_in_as_npub() -> Result<()> {
        with_fresh_config(|| {
            standard_login()?.exit()?;

            let mut p = CliTester::new(["login"]);

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

            let mut p = CliTester::new(["login"]);

            p.expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                .expect_password(EXPECTED_PASSWORD_PROMPT)?
                .succeeds_with(TEST_INVALID_PASSWORD)?;
            p.expect_end_with(format!("Error: failed to log in as {}\r\n\r\nCaused by:\r\n    0: failed to decrypt key with provided password\r\n    1: failed to decrypt\r\n", TEST_KEY_1_NPUB).as_str())
        })
    }
}

mod when_called_with_nsec_parameter_only {
    use super::*;

    #[test]
    #[serial]
    fn valid_nsec_param_succeeds_without_prompts() -> Result<()> {
        with_fresh_config(|| {
            CliTester::new(["login", "--nsec", TEST_KEY_1_NSEC])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
        })
    }

    #[test]
    #[serial]
    fn forgets_identity() -> Result<()> {
        with_fresh_config(|| {
            CliTester::new(["login", "--nsec", TEST_KEY_1_NSEC])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

            let mut p = CliTester::new(["login"]);

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

                CliTester::new(["login", "--nsec", TEST_KEY_2_NSEC])
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NPUB).as_str())
            })
        }
    }
    #[test]
    #[serial]
    fn invalid_nsec_param_fails_without_prompts() -> Result<()> {
        with_fresh_config(|| {
            CliTester::new(["login", "--nsec", TEST_INVALID_NSEC]).expect_end_with(
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
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
            ])
            .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

            CliTester::new(["login"])
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
                    "--nsec",
                    TEST_KEY_2_NSEC,
                    "--password",
                    TEST_PASSWORD,
                ])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NPUB).as_str())?;

                CliTester::new(["login"])
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
                    "--nsec",
                    TEST_KEY_1_NSEC,
                    "--password",
                    TEST_INVALID_PASSWORD,
                ])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

                CliTester::new(["--password", TEST_INVALID_PASSWORD, "login"])
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

            CliTester::new(["login", "--password", TEST_PASSWORD])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
        })
    }

    #[test]
    #[serial]
    fn when_no_nsec_stored_logs_error() -> Result<()> {
        with_fresh_config(|| {
            CliTester::new(["login", "--password", TEST_PASSWORD])
                .expect_end_with("Error: no nsec available to decrypt with specified password\r\n")
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
            let mut p = CliTester::new_with_timeout(10000, ["login"]);
            p.expect_input(EXPECTED_NSEC_PROMPT)?
                .succeeds_with(TEST_KEY_1_NSEC)?;

            p.expect_password(EXPECTED_SET_PASSWORD_PROMPT)?
                .with_confirmation(EXPECTED_SET_PASSWORD_CONFIRM_PROMPT)?
                .succeeds_with(TEST_WEAK_PASSWORD)?;

            p.expect("this may take a few seconds...\r\n")?;

            p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())?;

            p = CliTester::new_with_timeout(10000, ["login"]);

            p.expect(format!("login as {}\r\n", TEST_KEY_1_NPUB).as_str())?
                .expect_password(EXPECTED_PASSWORD_PROMPT)?
                .succeeds_with(TEST_WEAK_PASSWORD)?;

            p.expect("this may take a few seconds...\r\n")?;

            p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NPUB).as_str())
        })
    }
}
