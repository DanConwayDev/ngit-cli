use std::{fs::File, io::BufReader};

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
#[cfg(test)]
use mockall::*;
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

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[allow(clippy::module_name_repetitions)]
pub struct MyConfig {
    pub version: u8,
    pub users: Vec<UserRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub nsec: String,
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serial_test::serial;
    use test_utils::*;

    use super::*;

    mod load {
        use super::*;

        #[test]
        #[serial]
        fn when_config_file_doesnt_exist_defaults_are_returned() -> Result<()> {
            with_fresh_config(|| {
                assert_eq!(ConfigManager.load()?, MyConfig::default());

                Ok(())
            })
        }

        #[test]
        #[serial]
        fn when_config_file_exists_it_is_returned() -> Result<()> {
            with_fresh_config(|| {
                let c = ConfigManager;
                let new_config = MyConfig {
                    version: 255,
                    ..MyConfig::default()
                };
                c.save(&new_config)?;
                assert_eq!(c.load()?, new_config);

                Ok(())
            })
        }
    }

    mod save {
        use super::*;

        #[test]
        #[serial]
        fn when_config_file_doesnt_config_is_saved() -> Result<()> {
            with_fresh_config(|| {
                let c = ConfigManager;
                let new_config = MyConfig {
                    version: 255,
                    ..MyConfig::default()
                };
                c.save(&new_config)?;
                assert_eq!(c.load()?, new_config);

                Ok(())
            })
        }

        #[test]
        #[serial]
        fn when_config_file_exists_new_config_is_saved() -> Result<()> {
            with_fresh_config(|| {
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
                assert_eq!(c.load()?, new_config);

                Ok(())
            })
        }
    }
}
