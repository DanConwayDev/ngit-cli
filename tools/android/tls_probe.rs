//! Manual Android TLS probe; see docs/architecture/android-testing.md.
//! Import the production module so its loopback tests run unchanged on Android.
#[path = "../../src/lib/tls.rs"]
mod tls;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: android_tls HTTPS_URL"))?;
    let response = tls::http_client_builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(url)
        .send()
        .await?;
    println!("HTTPS response: {}", response.status());
    response.error_for_status()?;
    Ok(())
}
