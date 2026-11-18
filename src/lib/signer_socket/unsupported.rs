//! Non-Unix platforms retain ordinary signers but cannot open socket signers.
use std::path::PathBuf;

use anyhow::{Result, bail};
use nostr::prelude::{Event, PublicKey, UnsignedEvent};

#[derive(Debug)]
pub enum SocketSigner {}

impl SocketSigner {
    pub async fn connect(_path: PathBuf, _account: Option<PublicKey>) -> Result<Self> {
        bail!("local socket signing is not supported on this platform")
    }

    pub fn public_key(&self) -> PublicKey {
        match *self {}
    }

    pub async fn sign_event(&self, _unsigned: UnsignedEvent) -> Result<Event> {
        match *self {}
    }

    pub async fn nip44_encrypt(&self, _public_key: &PublicKey, _content: &str) -> Result<String> {
        match *self {}
    }

    pub async fn nip44_decrypt(&self, _public_key: &PublicKey, _payload: &str) -> Result<String> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connection_fails_without_opening_a_transport() {
        let result = SocketSigner::connect(PathBuf::from("unused.sock"), None).await;
        assert!(result.unwrap_err().to_string().contains("not supported"));
    }
}
