//! Nix-built Buzz relay integration fixture.
//!
//! The fixture launches the relay output exported by Buzz's Nix flake, backed
//! by isolated Postgres, Redis, and Garage subprocesses. Garage
//! is used only as a local single-writer S3-compatible test backend. Buzz's
//! production object-store conformance probe is disabled because Garage 2.3
//! does not provide the concurrent `If-Match` linearizability that probe
//! requires; the integration test still exercises the real sequential clone,
//! fetch, and push object-store paths.

use std::{
    env, fs,
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::sleep,
};

use crate::{port, query};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const READY_POLL: Duration = Duration::from_millis(100);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const GARAGE_ACCESS_KEY: &str = "GK0123456789abcdef01234567";
const GARAGE_SECRET_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const S3_BUCKET: &str = "buzz-ngit-test";
const GIT_HOOK_SECRET: &str = "0123456789abcdef0123456789abcdef";

struct ManagedProcess {
    name: &'static str,
    child: Child,
    log_path: PathBuf,
}

impl ManagedProcess {
    fn spawn(name: &'static str, command: &mut Command, log_path: PathBuf) -> Result<Self> {
        let stdout = File::create(&log_path)
            .with_context(|| format!("failed to create {} log", log_path.display()))?;
        let stderr = stdout
            .try_clone()
            .with_context(|| format!("failed to clone {} log handle", log_path.display()))?;
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn {name}"))?;
        Ok(Self {
            name,
            child,
            log_path,
        })
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_else(|error| {
            format!("<failed to read {}: {error}>", self.log_path.display())
        })
    }

    fn check_running(&mut self) -> Result<()> {
        if let Some(status) = self
            .child
            .try_wait()
            .with_context(|| format!("failed to poll {}", self.name))?
        {
            bail!(
                "{} exited before becoming ready ({status})\n{}",
                self.name,
                self.log()
            );
        }
        Ok(())
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A live Buzz relay and its isolated service dependencies.
pub struct BuzzServer {
    http_url: String,
    relay_url: String,
    owner_pubkey: PublicKey,
    owner: Keys,
    _relay: ManagedProcess,
    _garage: ManagedProcess,
    _redis: ManagedProcess,
    _postgres: ManagedProcess,
    _temp_dir: TempDir,
}

impl BuzzServer {
    /// Launch the Nix-built Buzz relay and wait until its health endpoint is
    /// ready. The supplied identity is bootstrapped as the deployment owner.
    pub async fn start(owner: &Keys) -> Result<Self> {
        let buzz_relay = binary_from_env("BUZZ_RELAY_BIN")?;
        let temp_dir = TempDir::new().context("failed to allocate Buzz fixture tempdir")?;
        let root = temp_dir.path();

        let postgres_data = root.join("postgres-data");
        let postgres_socket = root.join("postgres-socket");
        let redis_data = root.join("redis");
        let garage_meta = root.join("garage-meta");
        let garage_data = root.join("garage-data");
        let git_data = root.join("git");
        let git_pack_cache = root.join("git-pack-cache");
        for path in [
            &postgres_socket,
            &redis_data,
            &garage_meta,
            &garage_data,
            &git_data,
            &git_pack_cache,
        ] {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }

        let postgres_port = port::reserve_port()?;
        let redis_port = port::reserve_port()?;
        let garage_s3_port = port::reserve_port()?;
        let garage_rpc_port = port::reserve_port()?;
        let relay_port = port::reserve_port()?;
        let health_port = port::reserve_port()?;
        let metrics_port = port::reserve_port()?;

        run_checked(
            "initdb",
            Command::new("initdb").arg("-D").arg(&postgres_data).args([
                "-A",
                "trust",
                "--no-locale",
                "--encoding=UTF8",
            ]),
        )?;

        let postgres_port_number = postgres_port.port();
        let mut postgres_command = Command::new("postgres");
        postgres_command
            .arg("-D")
            .arg(&postgres_data)
            .args(["-h", "127.0.0.1", "-p"])
            .arg(postgres_port_number.to_string())
            .arg("-k")
            .arg(&postgres_socket);
        let _ = postgres_port.release();
        let mut postgres =
            ManagedProcess::spawn("Postgres", &mut postgres_command, root.join("postgres.log"))?;
        wait_for_tcp(&mut postgres, postgres_port_number).await?;
        wait_for_command(&mut postgres, || {
            Command::new("pg_isready")
                .args(["-h", "127.0.0.1", "-p"])
                .arg(postgres_port_number.to_string())
                .output()
        })
        .await?;
        run_checked(
            "createuser",
            Command::new("createuser")
                .args(["-h", "127.0.0.1", "-p"])
                .arg(postgres_port_number.to_string())
                .arg("buzz"),
        )?;
        run_checked(
            "createdb",
            Command::new("createdb")
                .args(["-h", "127.0.0.1", "-p"])
                .arg(postgres_port_number.to_string())
                .args(["-O", "buzz"])
                .arg("buzz"),
        )?;

        let redis_port_number = redis_port.port();
        let mut redis_command = Command::new("redis-server");
        redis_command
            .args(["--bind", "127.0.0.1", "--port"])
            .arg(redis_port_number.to_string())
            .args(["--save", "", "--appendonly", "no", "--dir"])
            .arg(&redis_data);
        let _ = redis_port.release();
        let mut redis = ManagedProcess::spawn("Redis", &mut redis_command, root.join("redis.log"))?;
        wait_for_tcp(&mut redis, redis_port_number).await?;

        let garage_s3_port_number = garage_s3_port.port();
        let garage_rpc_port_number = garage_rpc_port.port();
        let garage_config = root.join("garage.toml");
        fs::write(
            &garage_config,
            format!(
                r#"metadata_dir = "{}"
data_dir = "{}"
db_engine = "lmdb"
replication_factor = 1
consistency_mode = "consistent"
rpc_bind_addr = "127.0.0.1:{garage_rpc_port_number}"
rpc_public_addr = "127.0.0.1:{garage_rpc_port_number}"
rpc_secret = "5c1915fa04d0b6739675c61bf5907eb0fe3d9c69850c83820f51b4d25d13868c"

[s3_api]
s3_region = "garage"
api_bind_addr = "127.0.0.1:{garage_s3_port_number}"
root_domain = ".s3.garage"
"#,
                garage_meta.display(),
                garage_data.display(),
            ),
        )
        .with_context(|| format!("failed to write {}", garage_config.display()))?;

        let mut garage_command = Command::new("garage");
        garage_command.arg("-c").arg(&garage_config).arg("server");
        let _ = garage_s3_port.release();
        let _ = garage_rpc_port.release();
        let mut garage =
            ManagedProcess::spawn("Garage", &mut garage_command, root.join("garage.log"))?;
        wait_for_tcp(&mut garage, garage_s3_port_number).await?;
        wait_for_tcp(&mut garage, garage_rpc_port_number).await?;

        let node = garage_output(&garage_config, ["node", "id", "-q"])?;
        let node_id = String::from_utf8(node.stdout)
            .context("Garage node id is not UTF-8")?
            .trim()
            .split('@')
            .next()
            .context("Garage node id output was empty")?
            .to_string();
        garage_checked(
            &garage_config,
            ["layout", "assign", "-z", "local", "-c", "1G", &node_id],
        )?;
        garage_checked(&garage_config, ["layout", "apply", "--version", "1"])?;
        garage_checked(
            &garage_config,
            [
                "key",
                "import",
                GARAGE_ACCESS_KEY,
                GARAGE_SECRET_KEY,
                "-n",
                "buzz-ngit-test",
                "--yes",
            ],
        )?;
        garage_checked(&garage_config, ["bucket", "create", S3_BUCKET])?;
        garage_checked(
            &garage_config,
            [
                "bucket",
                "allow",
                "--read",
                "--write",
                "--owner",
                S3_BUCKET,
                "--key",
                "buzz-ngit-test",
            ],
        )?;

        let relay_port_number = relay_port.port();
        let health_port_number = health_port.port();
        let metrics_port_number = metrics_port.port();
        let http_url = format!("http://127.0.0.1:{relay_port_number}");
        let relay_url = format!("ws://127.0.0.1:{relay_port_number}");
        let owner_pubkey = owner.public_key();
        let owner_secret = owner.secret_key().to_secret_hex();
        let database_url = format!("postgres://buzz@127.0.0.1:{postgres_port_number}/buzz");
        let redis_url = format!("redis://127.0.0.1:{redis_port_number}");
        let s3_endpoint = format!("http://127.0.0.1:{garage_s3_port_number}");

        let mut relay_command = Command::new(&buzz_relay);
        relay_command
            .current_dir(root)
            .env("BUZZ_BIND_ADDR", format!("127.0.0.1:{relay_port_number}"))
            .env("BUZZ_HEALTH_PORT", health_port_number.to_string())
            .env("BUZZ_METRICS_PORT", metrics_port_number.to_string())
            .env("DATABASE_URL", database_url)
            .env("REDIS_URL", redis_url)
            .env("BUZZ_AUTO_MIGRATE", "true")
            .env("BUZZ_DB_POOL_SIZE", "8")
            .env("BUZZ_REDIS_POOL_SIZE", "4")
            .env("BUZZ_REQUIRE_AUTH_TOKEN", "false")
            .env("BUZZ_REQUIRE_RELAY_MEMBERSHIP", "false")
            .env("RELAY_URL", &relay_url)
            .env("BUZZ_RELAY_PRIVATE_KEY", &owner_secret)
            .env("RELAY_OWNER_PUBKEY", owner_pubkey.to_hex())
            .env("BUZZ_S3_ENDPOINT", s3_endpoint)
            .env("BUZZ_S3_ACCESS_KEY", GARAGE_ACCESS_KEY)
            .env("BUZZ_S3_SECRET_KEY", GARAGE_SECRET_KEY)
            .env("BUZZ_S3_BUCKET", S3_BUCKET)
            .env("BUZZ_S3_REGION", "garage")
            .env("BUZZ_S3_ADDRESSING_STYLE", "path")
            .env("BUZZ_MEDIA_BASE_URL", format!("{http_url}/media"))
            .env("BUZZ_GIT_REPO_PATH", &git_data)
            .env("BUZZ_GIT_PACK_CACHE_PATH", &git_pack_cache)
            .env("BUZZ_GIT_PACK_CACHE_MAX_BYTES", "0")
            .env("BUZZ_GIT_HOOK_HMAC_SECRET", GIT_HOOK_SECRET)
            .env("BUZZ_GIT_CONFORMANCE_PROBE", "false")
            .env("BUZZ_AUDIT_ENABLED", "false")
            .env("BUZZ_HUDDLE_AUDIO_AVAILABLE", "false")
            .env("BUZZ_PUSH_GATEWAY_DELIVERY_URL", "")
            .env("RUST_LOG", "warn");
        let _ = relay_port.release();
        let _ = health_port.release();
        let _ = metrics_port.release();
        let mut relay = ManagedProcess::spawn(
            "Buzz relay",
            &mut relay_command,
            root.join("buzz-relay.log"),
        )?;
        wait_for_http_ready(&mut relay, health_port_number, "/_readiness").await?;

        Ok(Self {
            http_url,
            relay_url,
            owner_pubkey,
            owner: owner.clone(),
            _relay: relay,
            _garage: garage,
            _redis: redis,
            _postgres: postgres,
            _temp_dir: temp_dir,
        })
    }

    /// `http://127.0.0.1:<port>`, used as the Buzz CLI base and Git origin.
    pub fn http_url(&self) -> &str {
        &self.http_url
    }

    /// `ws://127.0.0.1:<port>`, used as the repository's Nostr relay.
    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    /// Create a private Buzz channel as the bootstrapped owner.
    pub async fn create_private_channel(&self, name: &str) -> Result<String> {
        // A fixed UUID is safe because each fixture owns an isolated database.
        let channel = "11111111-1111-4111-8111-111111111111".to_string();
        let event = EventBuilder::new(Kind::Custom(9007), "")
            .tags([
                Tag::parse(["h", channel.as_str()])?,
                Tag::parse(["name", name])?,
                Tag::parse(["visibility", "private"])?,
                Tag::parse(["channel_type", "stream"])?,
            ])
            .finalize(&self.owner)?;
        self.publish(event).await?;
        Ok(channel)
    }

    /// Announce an empty repository bound to `channel`, returning its Smart
    /// HTTP clone URL. Buzz creates the repository lazily from this event.
    pub async fn announce_repository(&self, identifier: &str, channel: &str) -> Result<String> {
        let clone_url = format!(
            "{}/git/{}/{}",
            self.http_url,
            self.owner_pubkey.to_hex(),
            identifier
        );
        let event = EventBuilder::new(Kind::Custom(30617), "")
            .tags([
                Tag::parse(["d", identifier])?,
                Tag::parse(["name", "ngit Buzz integration"])?,
                Tag::parse(["clone", clone_url.as_str()])?,
                Tag::parse(["relays", self.relay_url.as_str()])?,
                Tag::parse(["buzz-channel", channel])?,
            ])
            .finalize(&self.owner)?;
        self.publish(event).await?;
        Ok(clone_url)
    }

    /// Query the Buzz relay over an authenticated NIP-42 connection.
    pub async fn events_as(&self, keys: &Keys, filter: Filter) -> Result<Vec<Event>> {
        query::fetch_events_as(&self.relay_url, keys, filter).await
    }

    async fn publish(&self, event: Event) -> Result<()> {
        use nostr_sdk::{authenticator::SignerAuthenticator, client::ClientBuilder};

        let client = ClientBuilder::default()
            .authenticator(SignerAuthenticator::new(self.owner.clone()))
            .build();
        client.add_relay(&self.relay_url).await?;
        client.connect().await;
        let output = client.send_event(&event).await?;
        client.disconnect().await;
        if !output.failed.is_empty() {
            bail!(
                "Buzz relay rejected event {}: {:?}",
                event.id,
                output.failed
            );
        }
        Ok(())
    }
}

fn binary_from_env(name: &str) -> Result<PathBuf> {
    let path = env::var_os(name)
        .with_context(|| format!("{name} is not set; run the test inside `nix develop`"))?;
    let path = PathBuf::from(path);
    if !path.is_file() {
        bail!("{name} points to missing binary {}", path.display());
    }
    Ok(path)
}

fn run_checked(label: &str, command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("failed to spawn {label}"))?;
    if !output.status.success() {
        bail!(
            "{label} exited {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(output)
}

fn garage_output<const N: usize>(config: &Path, args: [&str; N]) -> Result<Output> {
    run_checked(
        "Garage CLI",
        Command::new("garage").arg("-c").arg(config).args(args),
    )
}

fn garage_checked<const N: usize>(config: &Path, args: [&str; N]) -> Result<()> {
    garage_output(config, args).map(|_| ())
}

async fn wait_for_tcp(process: &mut ManagedProcess, port: u16) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        process.check_running()?;
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "{} did not listen on 127.0.0.1:{port} within {READY_TIMEOUT:?}\n{}",
                process.name,
                process.log()
            );
        }
        sleep(READY_POLL).await;
    }
}

async fn wait_for_command(
    process: &mut ManagedProcess,
    mut probe: impl FnMut() -> std::io::Result<Output>,
) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        process.check_running()?;
        if probe().is_ok_and(|output| output.status.success()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "{} did not pass its readiness command within {READY_TIMEOUT:?}\n{}",
                process.name,
                process.log()
            );
        }
        sleep(READY_POLL).await;
    }
}

async fn wait_for_http_ready(process: &mut ManagedProcess, port: u16, path: &str) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        process.check_running()?;
        let probe = async {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await?;
            let mut response = [0_u8; 64];
            let read = stream.read(&mut response).await?;
            let status = String::from_utf8_lossy(&response[..read]);
            if status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200") {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "non-200 readiness response: {status:?}"
                )))
            }
        };
        if tokio::time::timeout(READY_PROBE_TIMEOUT, probe)
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "{} did not pass HTTP readiness within {READY_TIMEOUT:?}\n{}",
                process.name,
                process.log()
            );
        }
        sleep(READY_POLL).await;
    }
}
