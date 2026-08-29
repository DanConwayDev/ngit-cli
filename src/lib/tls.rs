//! Process-level Rustls cryptographic-provider selection.

/// Install Ring when the embedding process has not already selected a Rustls
/// cryptographic provider.
///
/// Reqwest's `rustls-no-provider` feature deliberately requires applications
/// to make this selection before building a client. Repeated and concurrent
/// calls are safe: a provider selected earlier by the application, or by a
/// racing caller, remains authoritative.
pub fn install_default_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Start a Reqwest client builder after satisfying its provider contract.
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    install_default_crypto_provider();
    reqwest::Client::builder()
}
