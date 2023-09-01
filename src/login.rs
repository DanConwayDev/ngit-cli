use anyhow::{Context, Result};

use crate::{
    config::{ConfigManagement, ConfigManager},
    key_handling::users::{UserManagement, UserManager},
};

pub fn launch(nsec: &Option<String>) -> Result<()> {
    let cfg = ConfigManager
        .load()
        .context("failed to load application config")?;
    if !cfg.users.is_empty() {
        println!("logged in as {}", cfg.users[0].nsec);
    }
    UserManager::default().add(nsec)
}
