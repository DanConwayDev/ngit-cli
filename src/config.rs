use std::{fs::File, io::BufReader};

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
#[cfg(test)]
use mockall::*;
use nostr::{PublicKey, ToBech32};
use serde::{self, Deserialize, Serialize};

#[derive(Default)]
#[allow(clippy::module_name_repetitions)]
pub struct ConfigManager;

#[cfg_attr(test, automock)]
#[allow(clippy::module_name_repetitions)]
pub trait ConfigManagement {
    fn load(&self) -> Result<MyConfig>;
    fn save(&self, cfg: &MyConfig) -> Result<()>;
}

pub fn get_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "CodeCollaboration", "ngit").ok_or(anyhow!(
        "should find operating system home directories with rust-directories crate"
    ))
}

impl ConfigManagement for ConfigManager {
    fn load(&self) -> Result<MyConfig> {
        let config_path = get_dirs()?.config_dir().join("config.json");
        if config_path.exists() {
            let file =
                File::open(config_path).context("should open application configuration file")?;
            let reader = BufReader::new(file);
            let config: MyConfig = serde_json::from_reader(reader)
                .context("should read config from config file with serde_json")?;
            Ok(config)
        } else {
            Ok(MyConfig::default())
        }
    }
    fn save(&self, cfg: &MyConfig) -> Result<()> {
        let dirs = get_dirs()?;
        let config_path = dirs.config_dir().join("config.json");
        let file = if config_path.exists() {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(config_path)
                .context(
                    "should open application configuration file with write and truncate options",
                )?
        } else {
            std::fs::create_dir_all(dirs.config_dir())
                .context("should create application config directories")?;
            std::fs::File::create(config_path).context("should create application config file")?
        };
        serde_json::to_writer_pretty(file, cfg)
            .context("should write configuration to config file with serde_json")
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::module_name_repetitions)]
pub struct MyConfig {
    pub version: u8,
    pub users: Vec<UserRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub public_key: PublicKey,
    pub encrypted_key: String,
    pub metadata: UserMetadata,
    pub relays: UserRelays,
    pub last_checked: u64,
}

impl UserRef {
    pub fn new(public_key: PublicKey, encrypted_key: String) -> Self {
        Self {
            public_key,
            encrypted_key,
            relays: UserRelays {
                relays: vec![],
                created_at: 0,
            },
            metadata: UserMetadata {
                #[allow(clippy::expect_used)]
                name: public_key
                    .to_bech32()
                    .expect("public key should always produce bech32"),
                // name: format!(
                //     "{}",
                //     public_key
                //         .to_bech32()
                //         .expect("public key should always produce bech32"),
                // )
                // .as_str()[..10].to_string(),
                created_at: 0,
            },
            last_checked: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserMetadata {
    pub name: String,
    pub created_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRelays {
    pub relays: Vec<UserRelayRef>,
    pub created_at: u64,
}

impl UserRelays {
    pub fn write(&self) -> Vec<String> {
        self.relays
            .iter()
            .filter(|r| r.write)
            .map(|r| r.url.clone())
            .collect()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRelayRef {
    pub url: String,
    pub read: bool,
    pub write: bool,
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serial_test::serial;

    use super::*;

    fn backup_existing_config() -> Result<()> {
        let config_path = get_dirs()?.config_dir().join("config.json");
        let backup_config_path = get_dirs()?.config_dir().join("config-backup.json");
        if config_path.exists() {
            std::fs::rename(config_path, backup_config_path)?;
        }
        Ok(())
    }

    fn restore_config_backup() -> Result<()> {
        let config_path = get_dirs()?.config_dir().join("config.json");
        let backup_config_path = get_dirs()?.config_dir().join("config-backup.json");
        if config_path.exists() {
            std::fs::remove_file(&config_path)?;
        }
        if backup_config_path.exists() {
            std::fs::rename(backup_config_path, config_path)?;
        }
        Ok(())
    }

    mod load {
        use super::*;

        #[test]
        #[serial]
        fn when_config_file_doesnt_exist_defaults_are_returned() -> Result<()> {
            backup_existing_config()?;
            let c = ConfigManager;
            assert_eq!(c.load()?, MyConfig::default());
            restore_config_backup()?;
            Ok(())
        }

        #[test]
        #[serial]
        fn when_config_file_exists_it_is_returned() -> Result<()> {
            backup_existing_config()?;
            let c = ConfigManager;
            let new_config = MyConfig {
                version: 255,
                ..MyConfig::default()
            };
            c.save(&new_config)?;
            assert_eq!(c.load()?, new_config);
            restore_config_backup()?;
            Ok(())
        }
    }

    mod save {
        use super::*;

        #[test]
        #[serial]
        fn when_config_file_doesnt_config_is_saved() -> Result<()> {
            backup_existing_config()?;
            let c = ConfigManager;
            let new_config = MyConfig {
                version: 255,
                ..MyConfig::default()
            };
            c.save(&new_config)?;
            assert_eq!(c.load().unwrap(), new_config);
            restore_config_backup()?;
            Ok(())
        }

        #[test]
        #[serial]
        fn when_config_file_exists_new_config_is_saved() -> Result<()> {
            backup_existing_config()?;
            let c = ConfigManager;
            let config = MyConfig {
                version: 255,
                ..MyConfig::default()
            };
            c.save(&config)?;
            let new_config = MyConfig {
                version: 254,
                ..MyConfig::default()
            };
            c.save(&new_config)?;
            assert_eq!(c.load().unwrap(), new_config);
            restore_config_backup()?;
            Ok(())
        }
    }
}
