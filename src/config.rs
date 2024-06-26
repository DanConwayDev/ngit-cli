use anyhow::{anyhow, Result};
use directories::ProjectDirs;
use nostr::PublicKey;
use serde::{self, Deserialize, Serialize};

pub fn get_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "CodeCollaboration", "ngit").ok_or(anyhow!(
        "should find operating system home directories with rust-directories crate"
    ))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub public_key: PublicKey,
    pub metadata: UserMetadata,
    pub relays: UserRelays,
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
