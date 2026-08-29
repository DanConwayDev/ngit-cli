//! Regression coverage for Reqwest's provider-neutral Rustls integration.

use anyhow::Result;
use rustls::crypto::CryptoProvider;

#[test]
fn explicit_ring_setup_supports_reqwest_clients_and_is_idempotent() -> Result<()> {
    assert!(
        CryptoProvider::get_default().is_none(),
        "this single-test process must begin without an implicit provider",
    );

    ngit::tls::install_default_crypto_provider();
    let installed = CryptoProvider::get_default().expect("Ring provider was not installed");
    reqwest::Client::builder().build()?;

    ngit::tls::install_default_crypto_provider();
    let after_repeat = CryptoProvider::get_default().expect("provider disappeared");
    assert!(
        std::sync::Arc::ptr_eq(installed, after_repeat),
        "repeated initialization must preserve the process provider",
    );
    Ok(())
}
