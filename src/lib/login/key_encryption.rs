use anyhow::Result;
use nostr::prelude::*;

pub fn decrypt_key(encrypted_key: &str, password: &str) -> Result<nostr::Keys> {
    let encrypted_key = nostr::nips::nip49::EncryptedSecretKey::from_bech32(encrypted_key)?;
    // to request that log_n gets exposed
    if encrypted_key.log_n() > 14 {
        println!("this may take a few seconds...");
    }
    Ok(nostr::Keys::new(encrypted_key.decrypt(password)?))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use once_cell::sync::Lazy;

    use super::*;

    // Locally-defined fixtures (previously imported from the `test_utils`
    // crate). Keep these values stable: TEST_KEY_1_ENCRYPTED /
    // TEST_KEY_1_ENCRYPTED_WEAK are precomputed ciphertexts of
    // TEST_KEY_1_NSEC under TEST_PASSWORD / TEST_WEAK_PASSWORD respectively,
    // so the "ciphertext has not silently changed format" regression tests
    // below can verify them.
    static TEST_KEY_1_NSEC: &str =
        "nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq";
    static TEST_KEY_1_ENCRYPTED: &str = "ncryptsec1qgq77e3uftz8dh3jkjxwdms3v6gwqaqduxyzld82kskas8jcs5xup3sf2pc5tr0erqkqrtu0ptnjgjlgvx8lt7c0d7laryq2u7psfa6zm7mk7ln3ln58468shwatm7cx5wy5wvm7yk74ksrngygwxg74";
    static TEST_KEY_1_ENCRYPTED_WEAK: &str = "ncryptsec1qg835almhlrmyxqtqeva44d5ugm9wk2ccmwspxrqv4wjsdpdlud9es5hsrvs0pas7dvsretm0mc26qwfc7v8986mqngnjshcplnqzj62lxf44a0kkdv788f6dh20x2eum96l2j8v37s5grrheu2hgrkf";
    static TEST_PASSWORD: &str = "769dfd£pwega8SHGv3!#Bsfd5t";
    static TEST_WEAK_PASSWORD: &str = "fhaiuhfwe";

    static TEST_KEY_1_KEYS: Lazy<nostr::Keys> =
        Lazy::new(|| nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap());

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
            keys.secret_key(),
            password,
            log2_rounds,
            KeySecurity::Medium,
        )?
        .to_bech32()?)
    }

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
            format!("{}", TEST_KEY_1_KEYS.secret_key().to_bech32().unwrap()),
            format!("{}", decrypted_key.secret_key().to_bech32().unwrap()),
        );
        Ok(())
    }

    #[test]
    // ensures password encryption hasn't changed
    fn decrypts_with_weak_password_from_reference_string() -> Result<()> {
        let decrypted_key = decrypt_key(TEST_KEY_1_ENCRYPTED_WEAK, TEST_WEAK_PASSWORD)?;

        assert_eq!(
            format!("{}", TEST_KEY_1_KEYS.secret_key().to_bech32().unwrap()),
            format!("{}", decrypted_key.secret_key().to_bech32().unwrap()),
        );
        Ok(())
    }

    #[test]
    fn decrypts_key_encrypted_using_encrypt_key() -> Result<()> {
        let key = nostr::Keys::generate();
        let s = encrypt_key(&key, TEST_PASSWORD)?;
        let newkey = decrypt_key(s.as_str(), TEST_PASSWORD)?;

        assert_eq!(
            format!("{}", key.secret_key().to_bech32().unwrap()),
            format!("{}", newkey.secret_key().to_bech32().unwrap()),
        );
        Ok(())
    }

    #[test]
    fn decrypt_key_successfully_decrypts_key_encrypted_using_encrypt_key() -> Result<()> {
        let key = nostr::Keys::generate();
        let s = encrypt_key(&key, TEST_PASSWORD)?;
        let newkey = decrypt_key(s.as_str(), TEST_PASSWORD)?;

        assert_eq!(
            format!("{}", key.secret_key().to_bech32().unwrap()),
            format!("{}", newkey.secret_key().to_bech32().unwrap()),
        );
        Ok(())
    }
}
