use anyhow::Result;
use serial_test::serial;
use test_utils::*;

static EXPECTED_NSEC_PROMPT: &str = "login with nsec (or hex private key)";

fn standard_login() -> Result<CliTester> {
    let mut p = CliTester::new(["login"]);

    p.expect_input_eventually(EXPECTED_NSEC_PROMPT)?
        .succeeds_with(TEST_KEY_1_NSEC)?;

    p.expect_end_eventually()?;
    Ok(p)
}

mod when_first_time_login {
    use super::*;

    #[test]
    #[serial]
    fn prompts_for_nsec() -> Result<()> {
        with_fresh_config(|| {
            standard_login()?;
            Ok(())
        })
    }

    #[test]
    #[serial]
    fn succeeds_with_text_logged_in_as_npub() -> Result<()> {
        with_fresh_config(|| {
            let mut p = CliTester::new(["login"]);

            p.expect_input(EXPECTED_NSEC_PROMPT)?
                .succeeds_with(TEST_KEY_1_NSEC)?;

            p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_1_NSEC).as_str())
        })
    }

    #[test]
    #[serial]
    fn next_time_returns_logged_in_as_npub() -> Result<()> {
        with_fresh_config(|| {
            standard_login()?.exit()?;

            CliTester::new(["login"])
                .expect(format!("logged in as {}\r\n", TEST_KEY_1_NSEC).as_str())?
                .exit()
        })
    }
}

mod when_called_with_nsec_parameter {
    use super::*;

    #[test]
    #[serial]
    fn valid_nsec_param_succeeds_without_prompts() -> Result<()> {
        with_fresh_config(|| {
            CliTester::new(["--nsec", TEST_KEY_2_NSEC, "login"])
                .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())?;

            CliTester::new(["login"])
                .expect(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())?
                .exit()
        })
    }

    mod when_logging_in_as_different_nsec {
        use super::*;

        #[test]
        #[serial]
        fn valid_nsec_param_succeeds_without_prompts_and_logs_in() -> Result<()> {
            with_fresh_config(|| {
                standard_login()?.exit()?;

                CliTester::new(["--nsec", TEST_KEY_2_NSEC, "login"])
                    .expect(format!("logged in as {}\r\n", TEST_KEY_1_NSEC).as_str())?
                    .expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())?;

                CliTester::new(["login"])
                    .expect(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())?
                    .exit()
            })
        }
    }
}

mod when_logged_in {
    use super::*;

    #[test]
    #[serial]
    fn returns_logged_in_as_npub() -> Result<()> {
        with_fresh_config(|| {
            standard_login()?.exit()?;

            CliTester::new(["login"])
                .expect(format!("logged in as {}\r\n", TEST_KEY_1_NSEC).as_str())?
                .exit()
        })
    }

    #[test]
    #[serial]
    fn prompts_to_log_in_with_different_nsec() -> Result<()> {
        with_fresh_config(|| {
            standard_login()?.exit()?;

            let mut p = CliTester::new(["login"]);
            p.expect(format!("logged in as {}\r\n", TEST_KEY_1_NSEC).as_str())?;

            p.expect_input(EXPECTED_NSEC_PROMPT)?
                .succeeds_with(TEST_KEY_2_NSEC)?;

            p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())
        })
    }
    mod when_logging_in_as_different_nsec {
        use super::*;

        #[test]
        #[serial]
        fn confirmed_as_logged_in_as_additional_user() -> Result<()> {
            with_fresh_config(|| {
                standard_login()?.exit()?;

                let mut p = CliTester::new(["login"]);
                p.expect(format!("logged in as {}\r\n", TEST_KEY_1_NSEC).as_str())?;

                p.expect_input(EXPECTED_NSEC_PROMPT)?
                    .succeeds_with(TEST_KEY_2_NSEC)?;

                p.expect_end_with(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())?;

                CliTester::new(["login"])
                    .expect(format!("logged in as {}\r\n", TEST_KEY_2_NSEC).as_str())?
                    .exit()
            })
        }
    }
}
