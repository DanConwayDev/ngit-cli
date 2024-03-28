use std::str::FromStr;

use anyhow::{anyhow, bail, ensure, Context, Result};
use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, AeadCore, KeyInit, OsRng, Payload},
    XChaCha20Poly1305,
};
#[cfg(test)]
use mockall::*;
use nostr::{prelude::*, Keys};
use nostr_sdk::bech32::{self, FromBase32, ToBase32};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use zeroize::Zeroize;

#[derive(Default)]
pub struct Encryptor;

#[cfg_attr(test, automock)]
pub trait EncryptDecrypt {
    /// requires less CPU time if the password is long
    fn encrypt_key(&self, keys: &Keys, password: &str) -> Result<String>;
    fn decrypt_key(&self, encrypted_key: &str, password: &str) -> Result<Keys>;
    /// generates a long random string
    fn random_token(&self) -> String;
}

/// approach and code adapted from nostr gossip client
impl EncryptDecrypt for Encryptor {
    fn encrypt_key(&self, keys: &Keys, password: &str) -> Result<String> {
        // Generate a random 16-byte salt
        let salt = {
            let mut salt: [u8; 16] = [0; 16];
            OsRng.fill_bytes(&mut salt);
            salt
        };

        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);

        let log2_rounds: u8 = if password.len() > 20 {
            // we have enough of entropy - no need to spend CPU time adding much more
            1
        } else {
            // default (scrypt::Params::RECOMMENDED_LOG_N) is 17 but 30s is too long to wait
            15
        };

        let associated_data: Vec<u8> = vec![1];

        let ciphertext = {
            let cipher = {
                let symmetric_key = password_to_key(password, &salt, log2_rounds)
                    .context("failed create encryption key from password")?;
                XChaCha20Poly1305::new((&symmetric_key).into())
            };
            cipher
                .encrypt(
                    &nonce,
                    Payload {
                        msg: keys
                            .secret_key()
                            .context(
                                "supplied key should reveal secret key. Is this a public key only?",
                            )?
                            .display_secret()
                            .to_string()
                            .as_bytes(),
                        aad: &associated_data,
                    },
                )
                .map_err(|_| anyhow!("ChaChaPoly1305 failed to encrypt nsec with password"))?
        };
        // Combine salt, IV and ciphertext
        let mut concatenation: Vec<u8> = Vec::new();
        concatenation.push(0x1); // 1 byte version number
        concatenation.push(log2_rounds); // 1 byte for scrypt N (rounds)
        concatenation.extend(salt); // 16 bytes of salt
        concatenation.extend(nonce); // 24 bytes of nonce
        concatenation.extend(associated_data); // 1 byte of key security
        concatenation.extend(ciphertext); // 48 bytes of ciphertext expected
        // Total length is 91 = 1 + 1 + 16 + 24 + 1 + 48

        bech32::encode(
            "ncryptsec",
            concatenation.to_base32(),
            bech32::Variant::Bech32,
        )
        .context("encrypted nsec failed to encode")
    }

    fn decrypt_key(&self, encrypted_key: &str, password: &str) -> Result<nostr::Keys> {
        let data =
            bech32::decode(encrypted_key).context("failed to decode encrypted key as bech32")?;
        if data.0 != "ncryptsec" {
            bail!("encrypted key is in the wrong format - it doesnt start with ncryptsec");
        }
        let concatenation = Vec::<u8>::from_base32(&data.1)
            .context("failed to convert bech32::decode output to Vec<u8>")?;

        // Break into parts
        let version: u8 = concatenation[0];
        ensure!(version == 0x1, "encryption version is incorrect");
        let log2_rounds: u8 = concatenation[1];
        let salt: [u8; 16] = concatenation[2..2 + 16].try_into()?;
        let nonce = &concatenation[2 + 16..2 + 16 + 24];
        let associated_data = &concatenation[(2 + 16 + 24)..=(2 + 16 + 24)];
        let ciphertext = &concatenation[2 + 16 + 24 + 1..];

        let cipher = {
            let symmetric_key = password_to_key(password, &salt, log2_rounds)?;
            XChaCha20Poly1305::new((&symmetric_key).into())
        };

        let payload = Payload {
            msg: ciphertext,
            aad: associated_data,
        };

        let mut inner_secret = cipher
            .decrypt(nonce.into(), payload)
            .map_err(|_| anyhow!("failed to decrypt"))?;

        if associated_data.is_empty() {
            bail!("invalid encrypted key");
        }

        let key =
            Keys::from_str(std::str::from_utf8(&inner_secret).context("inner secret is not [u8]")?)
                .context(
                    "incorrect password. Key decrypted with password did not produce a valid nsec.",
                )?;

        inner_secret.zeroize();

        Ok(key)
    }

    fn random_token(&self) -> String {
        thread_rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    }
}

/// uses scrypt to stretch password into key
fn password_to_key(password: &str, salt: &[u8; 16], log_n: u8) -> Result<[u8; 32]> {
    let params = scrypt::Params::new(log_n, 8, 1, 32)
        .context("scrypt failed to generate params to stretch password")?;
    let mut key: [u8; 32] = [0; 32];
    if log_n > 14 {
        println!("this may take a few seconds...");
    }

    scrypt::scrypt(password.as_bytes(), salt, &params, &mut key)
        .context("scrypt failed to stretch password")?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use test_utils::*;

    use super::*;

    #[test]
    fn encrypt_key_produces_string_prefixed_with() -> Result<()> {
        let s = Encryptor.encrypt_key(&nostr::Keys::generate(), TEST_PASSWORD)?;
        assert!(s.starts_with("ncryptsec"));
        Ok(())
    }

    #[test]
    // ensures password encryption hasn't changed
    fn decrypts_with_strong_password_from_reference_string() -> Result<()> {
        let encryptor = Encryptor;
        let decrypted_key = encryptor.decrypt_key(TEST_KEY_1_ENCRYPTED, TEST_PASSWORD)?;

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
        let encryptor = Encryptor;
        let decrypted_key = encryptor.decrypt_key(TEST_KEY_1_ENCRYPTED_WEAK, TEST_WEAK_PASSWORD)?;

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
        let encryptor = Encryptor;
        let key = nostr::Keys::generate();
        let s = encryptor.encrypt_key(&key, TEST_PASSWORD)?;
        let newkey = encryptor.decrypt_key(s.as_str(), TEST_PASSWORD)?;

        assert_eq!(
            format!("{}", key.secret_key().unwrap().to_bech32().unwrap()),
            format!("{}", newkey.secret_key().unwrap().to_bech32().unwrap()),
        );
        Ok(())
    }

    #[test]
    fn decrypt_key_successfully_decrypts_key_encrypted_using_encrypt_key() -> Result<()> {
        let encryptor = Encryptor;
        let key = nostr::Keys::generate();
        let s = encryptor.encrypt_key(&key, TEST_PASSWORD)?;
        let newkey = encryptor.decrypt_key(s.as_str(), TEST_PASSWORD)?;

        assert_eq!(
            format!("{}", key.secret_key().unwrap().to_bech32().unwrap()),
            format!("{}", newkey.secret_key().unwrap().to_bech32().unwrap()),
        );
        Ok(())
    }

    #[test]
    fn password_to_key_returns_ok_with_standard_password() {
        let salt = {
            let mut salt: [u8; 16] = [0; 16];
            OsRng.fill_bytes(&mut salt);
            salt
        };

        let log2_rounds: u8 = 1;

        assert!(password_to_key(TEST_PASSWORD, &salt, log2_rounds).is_ok());
    }
}
