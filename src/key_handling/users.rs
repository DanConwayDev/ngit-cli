use std::{str::FromStr, time::SystemTime};

use anyhow::{Context, Result};
use async_trait::async_trait;
use nostr::prelude::*;
use zeroize::Zeroize;

use super::encryption::{EncryptDecrypt, Encryptor};
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms, PromptPasswordParms},
    client::Connect,
    config::{
        self, ConfigManagement, ConfigManager, UserMetadata, UserRef, UserRelayRef, UserRelays,
    },
};

#[derive(Default)]
pub struct UserManager {
    config_manager: ConfigManager,
    interactor: Interactor,
    encryptor: Encryptor,
}

#[async_trait]
pub trait UserManagement {
    fn add(&self, nsec: &Option<String>, password: &Option<String>) -> Result<nostr::Keys>;
    async fn get_user(
        &self,
        #[cfg(test)] client: &MockConnect,
        #[cfg(not(test))] client: &Client,
        public_key: &PublicKey,
        after: u64,
    ) -> Result<UserRef>;
    fn get_user_from_cache(&self, public_key: &PublicKey) -> Result<UserRef>;
    fn add_user_to_config(
        &self,
        public_key: PublicKey,
        encrypted_secret_key: Option<String>,
        overwrite: bool,
    ) -> Result<()>;
}

#[cfg(test)]
use duplicate::duplicate_item;
#[cfg_attr(test, duplicate_item(UserManager; [UserManager]; [self::tests::MockUserManager]))]
#[async_trait]
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
            match Keys::from_str(&pk) {
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

        self.add_user_to_config(keys.public_key(), Some(encrypted_secret_key), true)?;

        Ok(keys)
    }

    fn add_user_to_config(
        &self,
        public_key: PublicKey,
        encrypted_secret_key: Option<String>,
        overwrite: bool,
    ) -> Result<()> {
        let user_ref = config::UserRef::new(public_key, encrypted_secret_key.unwrap_or_default());

        let mut cfg = self.config_manager.load().context("failed to load application config to find and remove any old versions of the user's encrypted key")?;
        // don't overwrite unless specified
        if !overwrite
            && cfg
                .users
                .clone()
                .into_iter()
                .any(|r| r.public_key.eq(&public_key))
        {
            return Ok(());
        }
        // if overwrite remove any duplicate entries for key before adding it to config
        cfg.users = cfg
            .users
            .clone()
            .into_iter()
            .filter(|r| !r.public_key.eq(&public_key))
            .collect();
        cfg.users.push(user_ref);
        self.config_manager
            .save(&cfg)
            .context("failed to save application configuration with new user details in")
    }

    fn get_user_from_cache(&self, public_key: &PublicKey) -> Result<UserRef> {
        let cfg = self
            .config_manager
            .load()
            .context("failed to load application config")?;
        Ok(cfg
            .users
            .iter()
            .find(|u| u.public_key.eq(public_key))
            .context(format!("pubkey isn't a current user: {public_key}"))?
            .clone())
    }
    /// get UserRef fetching most recent user relays and metadata infomation
    /// from
    #[allow(clippy::too_many_lines)]
    async fn get_user(
        &self,
        #[cfg(test)] client: &MockConnect,
        #[cfg(not(test))] client: &Client,
        public_key: &PublicKey,
        use_cache_unless_checked_more_than_x_secs_ago: u64,
    ) -> Result<UserRef> {
        let cfg = self
            .config_manager
            .load()
            .context("failed to load application config")?;
        let mut user_ref = cfg
            .users
            .iter()
            .find(|u| u.public_key.eq(public_key))
            .context(format!("pubkey isn't a current user: {public_key}"))?
            .clone();
        // return cache if last fetched was within X minutes
        if !unix_timestamp_after_now_plus_secs(
            user_ref.last_checked,
            use_cache_unless_checked_more_than_x_secs_ago,
        ) {
            return Ok(user_ref);
        }

        let mut relays_to_search = if user_ref.relays.write().is_empty() {
            client.get_fallback_relays().clone()
        } else {
            user_ref.relays.write()
        };

        let mut relays_searched: Vec<String> = vec![];

        loop {
            for r in &relays_to_search {
                if !relays_searched.iter().any(|sr| r.eq(sr)) {
                    relays_searched.push(r.clone());
                }
            }

            let events: Vec<Event> = match client
                .get_events(
                    relays_to_search,
                    vec![
                        nostr::Filter::default()
                            .author(*public_key)
                            .since(nostr::Timestamp::from(user_ref.metadata.created_at + 1))
                            .kind(Kind::Metadata),
                        nostr::Filter::default()
                            .author(*public_key)
                            .since(nostr::Timestamp::from(user_ref.relays.created_at + 1))
                            .kind(Kind::RelayList),
                    ],
                )
                .await
            {
                Ok(events) => events,
                Err(_) => {
                    return Ok(user_ref.clone());
                }
            };

            user_ref.last_checked = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("system time should be after the year 1970")?
                .as_secs();

            if let Some(new_metadata_event) = events
                .iter()
                .filter(|e| e.kind.eq(&nostr::Kind::Metadata) && e.pubkey.eq(public_key))
                .max_by_key(|e| e.created_at)
            {
                if new_metadata_event.created_at.as_u64() > user_ref.metadata.created_at {
                    let metadata = nostr::Metadata::from_json(new_metadata_event.content.clone())
                        .context("metadata cannot be found in kind 0 event content")?;
                    user_ref.metadata = UserMetadata {
                        name: if let Some(n) = metadata.name {
                            n
                        } else if let Some(n) = metadata.custom.get("displayName") {
                            // strip quote marks that custom.get() adds
                            let binding = n.to_string();
                            let mut chars = binding.chars();
                            chars.next();
                            chars.next_back();
                            chars.as_str().to_string()
                        } else if let Some(n) = metadata.display_name {
                            n
                        } else {
                            user_ref.metadata.name
                        },
                        created_at: new_metadata_event.created_at.as_u64(),
                    };
                }
            };

            if let Some(new_relays_event) = events
                .iter()
                .filter(|e| e.kind.eq(&nostr::Kind::RelayList) && e.pubkey.eq(public_key))
                .max_by_key(|e| e.created_at)
            {
                if new_relays_event.created_at.as_u64() > user_ref.relays.created_at {
                    let new_relay_list = UserRelays {
                        relays: new_relays_event
                            .tags
                            .iter()
                            .filter(|t| {
                                t.kind().eq(&nostr::TagKind::SingleLetter(
                                    SingleLetterTag::lowercase(Alphabet::R),
                                ))
                            })
                            .map(|t| UserRelayRef {
                                url: t.as_vec()[1].clone(),
                                read: t.as_vec().len() == 2 || t.as_vec()[2].eq("read"),
                                write: t.as_vec().len() == 2 || t.as_vec()[2].eq("write"),
                            })
                            .collect(),
                        created_at: new_relays_event.created_at.as_u64(),
                    };
                    let new_relays: Vec<String> = new_relay_list
                        .write()
                        .iter()
                        .filter(|r| !relays_searched.iter().any(|or| r.eq(&or)))
                        .map(std::clone::Clone::clone)
                        .collect();
                    user_ref.relays = new_relay_list;

                    if !new_relays.is_empty() {
                        relays_to_search = new_relays;
                        continue;
                    }
                }
            };

            // remove any duplicate entries for key before adding it to config
            let mut cfg = self.config_manager.load().context("failed to load application config to find and remove any old versions of the user's encrypted key")?;
            cfg.users = cfg
                .users
                .clone()
                .into_iter()
                .filter(|r| !r.public_key.eq(public_key))
                .collect();
            cfg.users.push(user_ref.clone());
            self.config_manager
                .save(&cfg)
                .context("failed to save application configuration with new user details in")?;
            break;
        }
        Ok(user_ref)
    }
}

fn unix_timestamp_after_now_plus_secs(timestamp: u64, secs: u64) -> bool {
    if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        now.as_secs() > (timestamp + secs)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use nostr;
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
                        k.eq(&Keys::from_str(TEST_KEY_1_NSEC).unwrap()) && p.eq(TEST_PASSWORD)
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
                            users: vec![UserRef::new(
                                TEST_KEY_1_KEYS.public_key(),
                                TEST_KEY_2_ENCRYPTED.into(),
                            )],
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
                            users: vec![UserRef::new(
                                TEST_KEY_2_KEYS.public_key(),
                                TEST_KEY_2_ENCRYPTED.into(),
                            )],
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

    fn now_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
    fn roughly_now(timestamp: u64) -> bool {
        let now = now_timestamp();
        timestamp < now + 100 && timestamp > now - 100
    }

    mod get_user {
        use anyhow::anyhow;

        use super::*;
        use crate::client::MockConnect;

        fn generate_relaylist_event() -> nostr::Event {
            nostr::event::EventBuilder::new(
                nostr::Kind::RelayList,
                "",
                [
                    nostr::Tag::RelayMetadata(
                        "wss://fredswrite1.relay".into(),
                        Some(nostr::RelayMetadata::Write),
                    ),
                    nostr::Tag::RelayMetadata(
                        "wss://fredsread1.relay".into(),
                        Some(nostr::RelayMetadata::Read),
                    ),
                    nostr::Tag::RelayMetadata("wss://fredsreadwrite.relay".into(), None),
                ],
            )
            .to_event(&TEST_KEY_1_KEYS)
            .unwrap()
        }

        fn generate_relaylist_event_user_2() -> nostr::Event {
            nostr::event::EventBuilder::new(
                nostr::Kind::RelayList,
                "",
                [
                    nostr::Tag::RelayMetadata(
                        "wss://carolswrite1.relay".into(),
                        Some(nostr::RelayMetadata::Write),
                    ),
                    nostr::Tag::RelayMetadata(
                        "wss://carolsread1.relay".into(),
                        Some(nostr::RelayMetadata::Read),
                    ),
                    nostr::Tag::RelayMetadata("wss://carolsreadwrite.relay".into(), None),
                ],
            )
            .to_event(&TEST_KEY_2_KEYS)
            .unwrap()
        }

        fn fallback_relays() -> Vec<String> {
            vec!["ws://fallback1".to_string(), "ws://fallback2".to_string()].clone()
        }

        fn generate_mock_client() -> MockConnect {
            let mut client = <MockConnect as std::default::Default>::default();
            client
                .expect_get_fallback_relays()
                .return_const(fallback_relays());
            client
        }

        fn generate_standard_config() -> MyConfig {
            MyConfig {
                users: vec![UserRef {
                    public_key: TEST_KEY_1_KEYS.public_key(),
                    encrypted_key: TEST_KEY_1_ENCRYPTED.to_string(),
                    metadata: UserMetadata {
                        name: "Fred".to_string(),
                        created_at: 10,
                    },
                    relays: UserRelays {
                        relays: vec![
                            UserRelayRef {
                                url: "ws://existingread".to_string(),
                                read: true,
                                write: false,
                            },
                            UserRelayRef {
                                url: "ws://existingreadwrite".to_string(),
                                read: true,
                                write: true,
                            },
                            UserRelayRef {
                                url: "ws://existingwrite".to_string(),
                                read: false,
                                write: true,
                            },
                        ],
                        created_at: 10,
                    },
                    last_checked: now_timestamp() - (60 * 60), // 1h ago
                }],
                ..MyConfig::default()
            }
            .clone()
        }

        fn expected_userrelayrefs_write1() -> UserRelayRef {
            UserRelayRef {
                url: "wss://fredswrite1.relay".into(),
                read: false,
                write: true,
            }
            .clone()
        }

        fn expected_userrelayrefs_read_write1() -> UserRelayRef {
            UserRelayRef {
                url: "wss://fredsreadwrite.relay".into(),
                read: true,
                write: true,
            }
            .clone()
        }

        fn expected_userrelayrefs() -> Vec<UserRelayRef> {
            vec![
                expected_userrelayrefs_write1(),
                UserRelayRef {
                    url: "wss://fredsread1.relay".into(),
                    read: true,
                    write: false,
                },
                expected_userrelayrefs_read_write1(),
            ]
        }

        mod when_within_caching_time_window {
            use super::*;

            #[tokio::test]
            async fn returns_cached_details_without_checking_relays_or_updaing_config() -> Result<()>
            {
                let mut m = MockUserManager::default();
                let client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                let res = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        24 * 60 * 60, // within 24 hours
                    )
                    .await?;
                assert_eq!(res.metadata.name, "Fred");
                assert_eq!(res.relays.relays[0].url, "ws://existingread");
                Ok(())
            }
        }

        mod returns_userref_with_latest_details_from_events_on_relays {
            use super::*;

            #[tokio::test]
            async fn name() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager.expect_save().returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .returning(|_, _| Ok(vec![generate_test_key_1_metadata_event("fred")]));

                let res = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                assert_eq!(res.metadata.name, "fred");
                Ok(())
            }

            #[tokio::test]
            async fn name_ignoring_other_users_events() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager.expect_save().returning(|_| Ok(()));
                client.expect_get_events().returning(|_, _| {
                    Ok(vec![
                        generate_test_key_2_metadata_event("carole"),
                        generate_test_key_1_metadata_event_old("fred"),
                    ])
                });

                let res = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                assert_eq!(res.metadata.name, "fred");
                Ok(())
            }

            #[tokio::test]
            async fn relays() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager.expect_save().returning(|_| Ok(()));
                client.expect_get_events().returning(|_, _| {
                    Ok(vec![
                        generate_test_key_1_metadata_event("fred"),
                        generate_relaylist_event(),
                    ])
                });

                let res = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                assert_eq!(res.relays.relays, expected_userrelayrefs(),);
                Ok(())
            }

            #[tokio::test]
            async fn relays_ignoring_other_users_events() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager.expect_save().returning(|_| Ok(()));
                client.expect_get_events().returning(|_, _| {
                    Ok(vec![
                        make_event_old_or_change_user(
                            generate_relaylist_event(),
                            &TEST_KEY_1_KEYS,
                            10000,
                        ),
                        generate_relaylist_event_user_2(),
                    ])
                });

                let res = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                assert_eq!(res.relays.relays, expected_userrelayrefs(),);
                Ok(())
            }
        }

        mod saves_updates_to_config {
            use super::*;

            #[tokio::test]
            async fn saves_name_to_config() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager
                    .expect_save()
                    .once()
                    .withf(|cfg| cfg.users[0].metadata.name.eq("fred"))
                    .returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .returning(|_, _| Ok(vec![generate_test_key_1_metadata_event("fred")]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            #[tokio::test]
            async fn updates_metadata_created_at() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager
                    .expect_save()
                    .once()
                    .withf(|cfg| roughly_now(cfg.users[0].metadata.created_at))
                    .returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .returning(|_, _| Ok(vec![generate_test_key_1_metadata_event("fred")]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            #[tokio::test]
            async fn saves_relays_to_config() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager
                    .expect_save()
                    .once()
                    .withf(|cfg| expected_userrelayrefs().eq(&cfg.users[0].relays.relays))
                    .returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .returning(|_, _| Ok(vec![generate_relaylist_event()]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            #[tokio::test]
            async fn updates_relays_created_at() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager
                    .expect_save()
                    .once()
                    .withf(|cfg| roughly_now(cfg.users[0].relays.created_at))
                    .returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .returning(|_, _| Ok(vec![generate_relaylist_event()]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            #[tokio::test]
            async fn when_no_changes_updates_last_updated() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager
                    .expect_save()
                    .once()
                    .withf(|cfg| roughly_now(cfg.users[0].last_checked))
                    .returning(|_| Ok(()));
                client.expect_get_events().returning(|_, _| Ok(vec![]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            #[tokio::test]
            async fn when_changes_updates_last_updated() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager
                    .expect_save()
                    .once()
                    .withf(|cfg| roughly_now(cfg.users[0].last_checked))
                    .returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .returning(|_, _| Ok(vec![generate_test_key_1_metadata_event("fred")]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }
        }

        mod fetches_from_correct_relays {
            use super::*;
            #[tokio::test]
            async fn when_userref_write_relays_present_fetches_only_from_them() -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager
                    .expect_load()
                    .returning(|| Ok(generate_standard_config()));
                m.config_manager.expect_save().returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .once()
                    .withf(move |relays, _filters| {
                        vec![
                            "ws://existingreadwrite".to_string(),
                            "ws://existingwrite".to_string(),
                        ]
                        .eq(relays)
                    })
                    .returning(|_, _| Ok(vec![]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            #[tokio::test]
            async fn when_userref_write_relays_not_present_fetches_from_fallback_relays()
            -> Result<()> {
                let mut m = MockUserManager::default();
                let mut client = generate_mock_client();
                m.config_manager.expect_load().returning(|| {
                    Ok(MyConfig {
                        users: vec![UserRef {
                            relays: UserRelays {
                                relays: vec![],
                                created_at: 0,
                            },
                            ..generate_standard_config().users[0].clone()
                        }],
                        ..generate_standard_config()
                    })
                });
                m.config_manager.expect_save().returning(|_| Ok(()));
                client
                    .expect_get_events()
                    .once()
                    .withf(move |relays, _filters| fallback_relays().eq(relays))
                    .returning(|_, _| Ok(vec![]));

                let _ = m
                    .get_user(
                        &client,
                        &TEST_KEY_1_KEYS.public_key(),
                        5 * 60, // 5 mins ago
                    )
                    .await?;
                Ok(())
            }

            mod fetches_from_new_relays_discovered_in_incoming_relay_list {
                use super::*;

                #[tokio::test]
                async fn when_all_relays_in_list_are_new_finds_name() -> Result<()> {
                    let mut m = MockUserManager::default();
                    let mut client = generate_mock_client();
                    m.config_manager.expect_load().returning(|| {
                        Ok(MyConfig {
                            users: vec![UserRef {
                                relays: UserRelays {
                                    relays: vec![],
                                    created_at: 0,
                                },
                                ..generate_standard_config().users[0].clone()
                            }],
                            ..generate_standard_config()
                        })
                    });
                    m.config_manager.expect_save().returning(|_| Ok(()));
                    client
                        .expect_get_events()
                        .times(2)
                        .withf(move |relays, _filters| {
                            fallback_relays().eq(relays)
                                || UserRelays {
                                    relays: expected_userrelayrefs(),
                                    created_at: 0,
                                }
                                .write()
                                .eq(relays)
                        })
                        .returning(|relays, _| {
                            if fallback_relays().eq(&relays) {
                                Ok(vec![generate_relaylist_event()])
                            } else if (UserRelays {
                                relays: expected_userrelayrefs(),
                                created_at: 0,
                            })
                            .write()
                            .eq(&relays)
                            {
                                Ok(vec![generate_test_key_1_metadata_event("fred")])
                            } else {
                                Ok(vec![])
                            }
                        });

                    let res = m
                        .get_user(
                            &client,
                            &TEST_KEY_1_KEYS.public_key(),
                            5 * 60, // 5 mins ago
                        )
                        .await?;
                    assert_eq!(res.metadata.name, "fred");
                    Ok(())
                }

                #[tokio::test]
                async fn only_fetches_from_newly_added_relays() -> Result<()> {
                    let mut m = MockUserManager::default();
                    let mut client = generate_mock_client();
                    m.config_manager.expect_load().returning(|| {
                        Ok(MyConfig {
                            users: vec![UserRef {
                                relays: UserRelays {
                                    relays: vec![expected_userrelayrefs_write1()],
                                    created_at: 0,
                                },
                                ..generate_standard_config().users[0].clone()
                            }],
                            ..generate_standard_config()
                        })
                    });
                    m.config_manager.expect_save().returning(|_| Ok(()));
                    client
                        .expect_get_events()
                        .times(2)
                        .withf(move |relays, _filters| {
                            vec![expected_userrelayrefs_write1().url].eq(relays)
                                || vec![expected_userrelayrefs_read_write1().url].eq(relays)
                        })
                        .returning(|relays, _| {
                            if vec![expected_userrelayrefs_write1().url].eq(&relays) {
                                Ok(vec![generate_relaylist_event()])
                            } else if vec![expected_userrelayrefs_read_write1().url].eq(&relays) {
                                Ok(vec![generate_test_key_1_metadata_event("fred")])
                            } else {
                                Ok(vec![])
                            }
                        });

                    let res = m
                        .get_user(
                            &client,
                            &TEST_KEY_1_KEYS.public_key(),
                            5 * 60, // 5 mins ago
                        )
                        .await?;
                    assert_eq!(res.metadata.name, "fred");
                    Ok(())
                }
            }
        }

        #[tokio::test]
        async fn when_failed_to_fetch_events_returns_cached_details() -> Result<()> {
            let mut m = MockUserManager::default();
            let mut client = generate_mock_client();
            m.config_manager
                .expect_load()
                .returning(|| Ok(generate_standard_config()));
            client
                .expect_get_events()
                .returning(|_, _| Err(anyhow!("test error")));

            let res = m
                .get_user(
                    &client,
                    &TEST_KEY_1_KEYS.public_key(),
                    5 * 60, // 10 mins ago
                )
                .await?;
            assert_eq!(res.metadata.name, "Fred");
            Ok(())
        }
    }
}
