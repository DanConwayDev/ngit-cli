//! Exercise the shipped helper pair against a local trusted/untrusted HTTPS Git
//! server. Set NGIT_TLS_TEST_BIN_DIR to an unpacked release archive to test
//! those exact bytes.

#![cfg(target_os = "linux")]

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use nostr_sdk::prelude::{Keys, ToBech32};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use test_harness::Harness;
use tokio::{net::TcpListener, task::JoinSet};

#[tokio::test(flavor = "multi_thread")]
async fn helper_pair_clones_git_https_and_rejects_untrusted_certificate() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(120), exercise()).await??;
    Ok(())
}

async fn exercise() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let ca = directory.path().join("ca.pem");
    let encoded = STANDARD.encode(include_bytes!("../src/lib/testdata/tls/ca.der"));
    std::fs::write(
        &ca,
        format!("-----BEGIN CERTIFICATE-----\n{encoded}\n-----END CERTIFICATE-----\n"),
    )?;
    let (ngit, helper) = match std::env::var_os("NGIT_TLS_TEST_BIN_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            (path.join("ngit"), path.join("git-remote-nostr"))
        }
        None => (
            PathBuf::from(env!("CARGO_BIN_EXE_ngit")),
            PathBuf::from(env!("CARGO_BIN_EXE_git-remote-nostr")),
        ),
    };
    let harness = Harness::builder(ngit, helper)
        .with_relay("default")
        .with_vanilla_git_server("https-backend")
        .with_child_env("SSL_CERT_FILE", ca.to_str().context("CA path")?)
        .with_child_env("GIT_SSL_CAINFO", ca.to_str().context("CA path")?)
        .build()
        .await?;
    let backend = harness.vanilla_git_server("https-backend");
    let upstream = backend.url().trim_start_matches("http://").to_string();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                include_bytes!("../src/lib/testdata/tls/server.der").to_vec(),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                include_bytes!("../src/lib/testdata/tls/server-key.der").to_vec(),
            )),
        )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let https_url = format!(
        "https://localhost:{}/repo.git",
        listener.local_addr()?.port()
    );
    // JoinSet drop aborts the listener and all connection tasks, including on
    // failure.
    let mut servers = JoinSet::<Result<()>>::new();
    servers.spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let acceptor = acceptor.clone();
                    let upstream = upstream.clone();
                    connections.spawn(async move {
                        if let Ok(mut tls) = acceptor.accept(stream).await {
                            let mut backend = tokio::net::TcpStream::connect(upstream).await?;
                            tokio::io::copy_bidirectional(&mut tls, &mut backend).await?;
                        }
                        Ok::<_, anyhow::Error>(())
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    let publisher = harness.fresh_repo()?;
    std::fs::write(
        publisher.dir().join("HTTPS.txt"),
        "verified HTTPS Git objects\n",
    )?;
    publisher
        .git_ok(["add", "HTTPS.txt"], "stage HTTPS fixture")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "seed HTTPS fixture", "--no-gpg-sign"],
            "commit fixture",
        )
        .await?;
    publisher
        .git_ok(["push", backend.url(), "main"], "seed HTTP backend")
        .await?;
    publisher
        .git_ok(["config", "nostr.nostate", "true"], "disable initial state")
        .await?;
    let keys = Keys::generate();
    let relay = harness.relay("default").url().to_string();
    let output = publisher
        .ngit([
            "init",
            "--name",
            "https-smoke",
            "--description",
            "local HTTPS fixture",
            "--additional-clone",
            &https_url,
            "--additional-relay",
            &relay,
            "--nsec",
            &keys.secret_key().to_bech32()?,
            "--defaults",
        ])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let url = format!(
        "nostr://{}/{}/https-smoke",
        keys.public_key().to_bech32()?,
        urlencoding::encode(&relay)
    );
    let cloned = harness.clone_url_with_git_config(&url, &[]).await?;
    ensure!(
        std::fs::read_to_string(cloned.dir().join("HTTPS.txt"))? == "verified HTTPS Git objects\n"
    );
    let untrusted = harness.fresh_repo()?;
    let unrelated_ca = directory.path().join("unrelated-ca.pem");
    let encoded = STANDARD.encode(webpki_root_certs::TLS_SERVER_ROOT_CERTS[0].as_ref());
    std::fs::write(
        &unrelated_ca,
        format!("-----BEGIN CERTIFICATE-----\n{encoded}\n-----END CERTIFICATE-----\n"),
    )?;
    let output = untrusted
        .git(["clone", &url, "rejected"])
        .env("SSL_CERT_FILE", &unrelated_ca)
        .env("SSL_CERT_DIR", directory.path())
        .env("GIT_SSL_CAINFO", &unrelated_ca)
        .output()
        .await?;
    ensure!(
        !output.status.success(),
        "untrusted Git HTTPS certificate was accepted"
    );
    ensure!(!untrusted.dir().join("rejected/HTTPS.txt").exists());
    Ok(())
}
