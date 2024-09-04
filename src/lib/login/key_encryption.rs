use anyhow::Result;
use nostr::{prelude::*, Keys};

pub fn encrypt_key(keys: &Keys, password: &str) -> Result<String> {
    let log2_rounds: u8 = if password.len() > 20 {
        // we have enough of entropy - no need to spend CPU time adding much more
        1
    } else {
        println!("this may take a few seconds...");
        // default (scrypt::Params::RECOMMENDED_LOG_N) is 17 but 30s is too long to wait
        15
    };
    Ok(nostr::nips::nip49::EncryptedSecretKey::new(
        keys.secret_key()?,
        password,
        log2_rounds,
        KeySecurity::Medium,
    )?
    .to_bech32()?)
}

pub fn decrypt_key(encrypted_key: &str, password: &str) -> Result<nostr::Keys> {
    let encrypted_key = nostr::nips::nip49::EncryptedSecretKey::from_bech32(encrypted_key)?;
    // to request that log_n gets exposed
    if encrypted_key.log_n() > 14 {
        println!("this may take a few seconds...");
    }
    Ok(nostr::Keys::new(encrypted_key.to_secret_key(password)?))
}

#[cfg(test)]
mod tests {
    use test_utils::*;

    use super::*;

    #[test]
    fn encrypt_key_produces_string_prefixed_with() -> Result<()> {
        let s = encrypt_key(&nostr::Keys::generate(), TEST_PASSWORD)?;
        assert!(s.starts_with("ncryptsec"));
        Ok(())
    }

    #[test]
    // ensures password encryption hasn't changed
    fn decrypts_with_strong_password_from_reference_string() -> Result<()> {
        let decrypted_key = decrypt_key(TEST_KEY_1_ENCRYPTED, TEST_PASSWORD)?;

        assert_eq!(
            format!(
                "{}",
                TEST_KEY_1_KEYS.secret_key().unwrap().to_bech32().unwrap()
            ),
            format!(
                "{}",
                decrypted_key.secret_key().unwrap().to_bech32().unwrap()
            ),
        );
        Ok(())
    }

    #[test]
    // ensures password encryption hasn't changed
    fn decrypts_with_weak_password_from_reference_string() -> Result<()> {
        let decrypted_key = decrypt_key(TEST_KEY_1_ENCRYPTED_WEAK, TEST_WEAK_PASSWORD)?;

        assert_eq!(
            format!(
                "{}",
                TEST_KEY_1_KEYS.secret_key().unwrap().to_bech32().unwrap()
            ),
            format!(
                "{}",
                decrypted_key.secret_key().unwrap().to_bech32().unwrap()
            ),
        );
        Ok(())
    }

    #[test]
    fn decrypts_key_encrypted_using_encrypt_key() -> Result<()> {
        let key = nostr::Keys::generate();
        let s = encrypt_key(&key, TEST_PASSWORD)?;
        let newkey = decrypt_key(s.as_str(), TEST_PASSWORD)?;

        assert_eq!(
            format!("{}", key.secret_key().unwrap().to_bech32().unwrap()),
            format!("{}", newkey.secret_key().unwrap().to_bech32().unwrap()),
        );
        Ok(())
    }

    #[test]
    fn decrypt_key_successfully_decrypts_key_encrypted_using_encrypt_key() -> Result<()> {
        let key = nostr::Keys::generate();
        let s = encrypt_key(&key, TEST_PASSWORD)?;
        let newkey = decrypt_key(s.as_str(), TEST_PASSWORD)?;

        assert_eq!(
            format!("{}", key.secret_key().unwrap().to_bech32().unwrap()),
            format!("{}", newkey.secret_key().unwrap().to_bech32().unwrap()),
        );
        Ok(())
    }
}
