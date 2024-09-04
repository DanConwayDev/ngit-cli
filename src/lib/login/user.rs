use nostr::PublicKey;
use nostr_sdk::Timestamp;
use serde::{self, Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub public_key: PublicKey,
    pub metadata: UserMetadata,
    pub relays: UserRelays,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserMetadata {
    pub name: String,
    pub created_at: Timestamp,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRelays {
    pub relays: Vec<UserRelayRef>,
    pub created_at: Timestamp,
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
