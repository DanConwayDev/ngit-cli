//! Rustls provider selection and HTTP certificate verification.

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
    let builder = reqwest::Client::builder();
    // Android's platform verifier requires a JVM and application context.
    // Standalone executables (including Termux) have neither. Use Mozilla's
    // bundled roots instead, preserving certificate and hostname validation.
    #[cfg(target_os = "android")]
    let builder = with_certificate_roots(builder, webpki_root_certs::TLS_SERVER_ROOT_CERTS);
    builder
}

#[cfg(any(target_os = "android", test))]
fn with_certificate_roots(
    builder: reqwest::ClientBuilder,
    roots: &[rustls::pki_types::CertificateDer<'_>],
) -> reqwest::ClientBuilder {
    builder.tls_certs_only(roots.iter().map(|cert| {
        // With our Rustls backend from_der stores DER without parsing it;
        // the client build validates the roots and returns any error.
        reqwest::Certificate::from_der(cert.as_ref()).expect("Rustls accepts DER certificates")
    }))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    const CA: &[u8] = include_bytes!("testdata/tls/ca.der");

    async fn request(
        roots: Option<&[CertificateDer<'_>]>,
        hostname: &str,
    ) -> reqwest::Result<reqwest::Response> {
        install_default_crypto_provider();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(
                    include_bytes!("testdata/tls/server.der").to_vec(),
                )],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    include_bytes!("testdata/tls/server-key.der").to_vec(),
                )),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let (stream, _) = listener.accept().await.unwrap();
            if let Ok(mut stream) = acceptor.accept(stream).await {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        let builder = http_client_builder();
        let builder = match roots {
            Some(roots) => with_certificate_roots(builder, roots),
            None => builder,
        };
        let result = builder
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .resolve(hostname, address)
            .build()
            .unwrap()
            .get(format!("https://{hostname}:{}/", address.port()))
            .send()
            .await;
        server.abort();
        result
    }

    #[tokio::test]
    async fn explicit_roots_accept_trusted_certificate() {
        assert!(
            request(Some(&[CertificateDer::from(CA)]), "localhost")
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }

    #[tokio::test]
    async fn explicit_roots_reject_wrong_hostname() {
        let error = request(Some(&[CertificateDer::from(CA)]), "wrong.invalid")
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        assert!(
            format!("{error:?}").contains("NotValidForName"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn bundled_roots_reject_untrusted_certificate() {
        let error = request(Some(webpki_root_certs::TLS_SERVER_ROOT_CERTS), "localhost")
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        assert!(format!("{error:?}").contains("UnknownIssuer"), "{error:?}");
    }

    // Exercise the production builder unchanged: on Android this must return
    // a certificate error rather than panic in the uninitialized JVM verifier.
    #[tokio::test]
    async fn default_client_rejects_untrusted_certificate_without_panicking() {
        let error = request(None, "localhost").await.unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        assert!(!error.is_timeout(), "{error:?}");
    }
}
