use anyhow::{Context, Result};
use nostr::prelude::*;
use zeroize::Zeroize;

use super::encryption::{EncryptDecrypt, Encryptor};
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms, PromptPasswordParms},
    config::{self, ConfigManagement, ConfigManager},
};

#[derive(Default)]
pub struct UserManager {
    config_manager: ConfigManager,
    interactor: Interactor,
    encryptor: Encryptor,
}

pub trait UserManagement {
    fn add(&self, nsec: &Option<String>, password: &Option<String>) -> Result<nostr::Keys>;
}

#[cfg(test)]
use duplicate::duplicate_item;
#[cfg_attr(test, duplicate_item(UserManager; [UserManager]; [self::tests::MockUserManager]))]
impl UserManagement for UserManager {
    fn add(&self, nsec: &Option<String>, password: &Option<String>) -> Result<nostr::Keys> {
        let mut prompt = "login with nsec (or hex private key)";
        let keys = loop {
            let pk = match nsec.clone() {
                Some(nsec) => nsec,
                None => self
                    .interactor
                    .input(PromptInputParms::default().with_prompt(prompt))
                    .context("failed to get nsec input from interactor")?,
            };
            match Keys::from_sk_str(&pk) {
                Ok(key) => {
                    break key;
                }
                Err(e) => {
                    if nsec.is_some() {
                        return Err(e).context(
                            "invalid nsec - supplied parameter could not be converted into a nostr private key",
                        );
                    }
                    prompt = "invalid nsec. try again with nsec (or hex private key)";
                }
            }
        };

        let mut pass = match password.clone() {
            Some(pass) => pass,
            None => self
                .interactor
                .password(
                    PromptPasswordParms::default()
                        .with_prompt("encrypt with password")
                        .with_confirm(),
                )
                .context("failed to get password input from interactor.password")?,
        };

        let encrypted_secret_key = self
            .encryptor
            .encrypt_key(&keys, &pass)
            .context("failed to encrypt nsec with password.")?;
        pass.zeroize();

        let user_ref = config::UserRef {
            public_key: keys.public_key(),
            encrypted_key: encrypted_secret_key,
        };

        // remove any duplicate entries for key before adding it to config
        let mut cfg = self.config_manager.load().context("failed to load application config to find and remove any old versions of the user's encrypted key")?;
        cfg.users = cfg
            .users
            .clone()
            .into_iter()
            .filter(|r| !r.public_key.eq(&keys.public_key()))
            .collect();
        cfg.users.push(user_ref);
        self.config_manager
            .save(&cfg)
            .context("failed to save application configuration with new user details in")?;

        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use test_utils::*;

    use super::*;
    use crate::{
        cli_interactor::MockInteractorPrompt,
        config::{MockConfigManagement, MyConfig, UserRef},
        key_handling::encryption::MockEncryptDecrypt,
    };

    #[derive(Default)]
    pub struct MockUserManager {
        pub config_manager: MockConfigManagement,
        pub interactor: MockInteractorPrompt,
        pub encryptor: MockEncryptDecrypt,
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
                self.interactor
                    .expect_password()
                    .returning(|_| Ok(TEST_PASSWORD.into()));
                self.encryptor
                    .expect_encrypt_key()
                    .returning(|_, _| Ok(TEST_KEY_1_ENCRYPTED.into()));
                self
            }
        }

        fn reuable_user_isnt_prompted(nsec: &str) {
            let mut m = MockUserManager::default().add_return_expected_responses();
            m.interactor = MockInteractorPrompt::default();
            m.interactor.expect_input().never();
            m.interactor.expect_password().never();
            let _ = m.add(&Some(nsec.into()), &Some(TEST_PASSWORD.to_string()));
        }

        fn reuable_config_isnt_modified(nsec: &str) {
            let mut m = MockUserManager::default();
            m.config_manager.expect_save().never();
            let _ = m.add(&Some(nsec.into()), &Some(TEST_PASSWORD.to_string()));
        }

        mod when_valid_nsec_and_password_is_passed {
            use super::*;

            #[test]
            fn user_isnt_prompted() {
                reuable_user_isnt_prompted(TEST_KEY_1_NSEC);
            }

            #[test]
            fn results_in_correct_keys() {
                let mut m = MockUserManager::default().add_return_expected_responses();
                m.interactor = MockInteractorPrompt::default();
                m.interactor.expect_input().never();
                m.interactor.expect_password().never();
                let r = m.add(
                    &Some(TEST_KEY_1_NSEC.into()),
                    &Some(TEST_PASSWORD.to_string()),
                );
                assert!(r.is_ok(), "should result in keys");
                assert!(
                    r.is_ok_and(|k| k
                        .secret_key()
                        .is_ok_and(|k| k.display_secret().to_string().eq(TEST_KEY_1_SK_HEX))),
                    "keys should reflect nsec"
                );
            }
        }
        mod when_invalid_nsec_is_passed_with_password {
            use super::*;

            #[test]
            fn user_isnt_prompted() {
                reuable_user_isnt_prompted(TEST_INVALID_NSEC);
            }

            #[test]
            fn config_isnt_modified() {
                reuable_config_isnt_modified(TEST_INVALID_NSEC);
            }

            #[test]
            fn results_in_an_error() {
                let m = MockUserManager::default();
                assert!(
                    m.add(
                        &Some(TEST_INVALID_NSEC.into()),
                        &Some(TEST_PASSWORD.to_string())
                    )
                    .is_err(),
                    "should result in an error"
                );
            }
        }
        mod when_no_nsec_is_passed {
            use super::*;

            #[test]
            fn prompt_for_nsec_and_password() {
                let mut m = MockUserManager::default().add_return_expected_responses();

                m.interactor = MockInteractorPrompt::new();
                m.interactor
                    .expect_input()
                    .once()
                    .withf(|p| p.prompt.eq("login with nsec (or hex private key)"))
                    .returning(|_| Ok(TEST_KEY_1_NSEC.into()));
                m.interactor
                    .expect_password()
                    .once()
                    .withf(|p| p.prompt.eq("encrypt with password"))
                    .returning(|_| Ok(TEST_KEY_1_NSEC.into()));

                let _ = m.add(&None, &None);
            }

            #[test]
            fn results_in_correct_keys() {
                let m = MockUserManager::default().add_return_expected_responses();

                let r = m.add(&None, &None);
                assert!(r.is_ok(), "should result in keys");
                assert!(
                    r.is_ok_and(|k| k
                        .secret_key()
                        .is_ok_and(|k| k.display_secret().to_string().eq(TEST_KEY_1_SK_HEX))),
                    "keys should reflect nsec"
                );
            }

            #[test]
            fn stores_encrypted_key_in_config() {
                let mut m = MockUserManager::default().add_return_expected_responses();

                m.config_manager = MockConfigManagement::new();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(MyConfig::default()));
                m.config_manager
                    .expect_save()
                    .withf(|cfg| {
                        cfg.users.len().eq(&1)
                            && cfg.users[0].encrypted_key.eq(TEST_KEY_1_ENCRYPTED)
                    })
                    .returning(|_| Ok(()));

                let _ = m.add(&None, &None);
            }

            #[test]
            fn stored_key_encrypted_with_password() {
                let mut m = MockUserManager::default().add_return_expected_responses();

                m.encryptor = MockEncryptDecrypt::new();
                m.encryptor
                    .expect_encrypt_key()
                    .once()
                    .withf(|k, p| {
                        k.eq(&Keys::from_sk_str(TEST_KEY_1_NSEC).unwrap()) && p.eq(TEST_PASSWORD)
                    })
                    .returning(|_, _| Ok(TEST_KEY_1_ENCRYPTED.into()));

                let _ = m.add(&None, &None);
            }

            mod when_user_key_already_stored {
                use super::*;
                use crate::config::UserRef;

                /// key overwritten as password may have changed
                #[test]
                fn key_not_saved_as_duplicate_but_encrypted_key_overwritten() {
                    let mut m = MockUserManager::default().add_return_expected_responses();

                    m.config_manager = MockConfigManagement::default();
                    m.config_manager.expect_load().returning(|| {
                        Ok(MyConfig {
                            users: vec![UserRef {
                                public_key: TEST_KEY_1_KEYS.public_key(),
                                // different key to TEST_KEY_1_ENCYPTED
                                encrypted_key: TEST_KEY_2_ENCRYPTED.into(),
                            }],
                            ..MyConfig::default()
                        })
                    });
                    m.config_manager
                        .expect_save()
                        .withf(|cfg| {
                            cfg.users.len() == 1
                                && cfg.users[0].encrypted_key.eq(TEST_KEY_1_ENCRYPTED)
                        })
                        .returning(|_| Ok(()));

                    let _ = m.add(&None, &None);
                }
            }

            mod when_multiple_users_added {
                use super::*;

                #[test]
                fn both_user_keys_are_stored() {
                    let mut m = MockUserManager::default().add_return_expected_responses();

                    m.config_manager = MockConfigManagement::default();
                    m.config_manager.expect_load().returning(|| {
                        Ok(MyConfig {
                            users: vec![UserRef {
                                public_key: TEST_KEY_2_KEYS.public_key(),
                                encrypted_key: TEST_KEY_2_ENCRYPTED.into(),
                            }],
                            ..MyConfig::default()
                        })
                    });
                    m.config_manager
                        .expect_save()
                        .withf(|cfg| {
                            cfg.users.len() == 2
                            // latest user stored at end of array
                            && cfg.users[1].encrypted_key.eq(TEST_KEY_1_ENCRYPTED)
                        })
                        .returning(|_| Ok(()));

                    let _ = m.add(&None, &None);
                }
            }
        }
    }
}
