use anyhow::{Context, Result};

use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    config::{ConfigManagement, ConfigManager, MyConfig, UserRef},
};

#[derive(Default)]
pub struct UserManager {
    config_manager: ConfigManager,
    interactor: Interactor,
}

pub trait UserManagement {
    fn add(&self, nsec: &Option<String>) -> Result<()>;
}

#[cfg(test)]
use duplicate::duplicate_item;
#[cfg_attr(test, duplicate_item(UserManager; [UserManager]; [self::tests::MockUserManager]))]
impl UserManagement for UserManager {
    fn add(&self, nsec: &Option<String>) -> Result<()> {
        let nsec = match nsec.clone() {
            Some(nsec) => nsec,
            None => self
                .interactor
                .input(
                    PromptInputParms::default().with_prompt("login with nsec (or hex private key)"),
                )
                .context("failed to get nsec input from interactor.input")?,
        };

        self.config_manager
            .save(&MyConfig {
                users: vec![UserRef {
                    nsec: nsec.to_string(),
                }],
                ..MyConfig::default()
            })
            .context("failed to save application configuration with new user details in")?;

        println!("logged in as {nsec}");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use test_utils::*;

    use super::*;
    use crate::{cli_interactor::MockInteractorPrompt, config::MockConfigManagement};

    #[derive(Default)]
    pub struct MockUserManager {
        pub config_manager: MockConfigManagement,
        pub interactor: MockInteractorPrompt,
    }

    mod add {
        use super::*;

        impl MockUserManager {
            fn add_return_expected_responses(mut self) -> Self {
                self.config_manager
                    .expect_load()
                    .returning(|| Ok(MyConfig::default()));
                self.config_manager.expect_save().returning(|_| Ok(()));
                self.interactor
                    .expect_input()
                    .returning(|_| Ok(TEST_KEY_1_NSEC.into()));
                self
            }
        }

        mod when_nsec_is_passed {
            use super::*;

            #[test]
            fn user_isnt_prompted() {
                let mut m = MockUserManager::default().add_return_expected_responses();
                m.interactor = MockInteractorPrompt::default();
                m.interactor.expect_input().never();

                let _ = m.add(&Some(TEST_KEY_1_NSEC.into()));
            }
        }

        mod when_no_nsec_is_passed {
            use super::*;

            #[test]
            fn prompt_for_nsec() {
                let mut m = MockUserManager::default().add_return_expected_responses();

                m.interactor = MockInteractorPrompt::new();
                m.interactor
                    .expect_input()
                    .once()
                    .withf(|p| p.prompt.eq("login with nsec (or hex private key)"))
                    .returning(|_| Ok(TEST_KEY_1_NSEC.into()));

                let _ = m.add(&None);
            }

            #[test]
            fn stored_in_config() {
                let mut m = MockUserManager::default().add_return_expected_responses();

                m.config_manager = MockConfigManagement::new();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(MyConfig::default()));
                m.config_manager
                    .expect_save()
                    .withf(|cfg| cfg.users.len().eq(&1) && cfg.users[0].nsec.eq(TEST_KEY_1_NSEC))
                    .returning(|_| Ok(()));

                let _ = m.add(&None);
            }
        }
    }
}
