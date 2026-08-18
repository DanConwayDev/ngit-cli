//! Nix-built Buzz relay integration fixture.
//!
//! The fixture launches the relay output exported by Buzz's Nix flake, backed
//! by isolated Postgres, Redis, and Garage subprocesses. Garage
//! is used only as a local single-writer S3-compatible test backend. Buzz's
//! production object-store conformance probe is disabled because Garage 2.3
//! does not provide the concurrent `If-Match` linearizability that probe
//! requires; the integration test still exercises the real sequential clone,
//! fetch, and push object-store paths.

#[cfg(target_os = "linux")]
use std::os::unix::{fs::MetadataExt, fs::chown, process::CommandExt};
use std::{
    env, fs,
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::sleep,
};

use crate::{
    port::{self, PortReservation},
    query,
};

/// Polling deadline shared by every readiness wait in this fixture (TCP
/// accept, `pg_isready`, HTTP readiness). Generous because cold CI runners
/// can spend a long time faulting in freshly downloaded Nix store paths; the
/// happy path is unaffected since each wait returns as soon as its probe
/// succeeds.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
const READY_POLL: Duration = Duration::from_millis(100);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How many fresh port reservations to attempt per subprocess before giving
/// up. Every service here binds its ports itself, so there is a
/// microsecond-scale TOCTOU window between [`PortReservation::release`] and
/// the subprocess's own `bind`. Losing that race makes the subprocess exit
/// before its readiness probe passes; [`spawn_with_bind_retry`] detects the
/// early exit and retries on freshly reserved ports instead of failing the
/// test. Matches the cap in `grasp.rs` and `relay.rs`.
const MAX_BIND_ATTEMPTS: usize = 5;
const POSTGRES_SUPERUSER: &str = "postgres";
const GARAGE_ACCESS_KEY: &str = "GK0123456789abcdef01234567";
const GARAGE_SECRET_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const S3_BUCKET: &str = "buzz-ngit-test";
const GIT_HOOK_SECRET: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    uid: u32,
    gid: u32,
}

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

    fn check_running(&mut self) -> std::result::Result<(), WaitFailure> {
        match self.child.try_wait() {
            Ok(Some(status)) => Err(WaitFailure::EarlyExit {
                name: self.name,
                status,
                log: self.log(),
            }),
            Ok(None) => Ok(()),
            Err(error) => Err(WaitFailure::Other(
                anyhow::Error::from(error).context(format!("failed to poll {}", self.name)),
            )),
        }
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
    _postgres_temp_dir: Option<TempDir>,
    _temp_dir: TempDir,
}

impl BuzzServer {
    /// Launch the Nix-built Buzz relay and wait until its health endpoint is
    /// ready. The supplied identity is bootstrapped as the deployment owner.
    ///
    /// Returns `Ok(None)` when the pinned Buzz binary is unavailable and the
    /// `CI` environment variable is unset — the caller should treat that as
    /// a skipped test. In CI a missing binary is a hard error so coverage
    /// cannot silently disappear.
    pub async fn start(owner: &Keys) -> Result<Option<Self>> {
        let Some(buzz_relay) = binary_from_env("BUZZ_RELAY_BIN")? else {
            return Ok(None);
        };
        let temp_dir = TempDir::new().context("failed to allocate Buzz fixture tempdir")?;
        let root = temp_dir.path();

        let postgres_identity = postgres_process_identity()?;
        // Nix shells commonly set TMPDIR to a caller-only directory. A
        // PostgreSQL child running under a different UID cannot traverse that
        // parent even when its own fixture directory is chowned. Root jobs
        // therefore give PostgreSQL a private directory directly below the
        // system /tmp, while all other fixture state remains in `temp_dir`.
        let postgres_temp_dir = if postgres_identity.is_some() {
            Some(
                TempDir::new_in("/tmp")
                    .context("failed to allocate unprivileged PostgreSQL fixture tempdir")?,
            )
        } else {
            None
        };
        let postgres_root = postgres_temp_dir.as_ref().map_or(root, TempDir::path);
        let postgres_data = postgres_root.join("postgres-data");
        let postgres_socket = postgres_root.join("postgres-socket");
        let redis_data = root.join("redis");
        let garage_meta = root.join("garage-meta");
        let garage_data = root.join("garage-data");
        let git_data = root.join("git");
        let git_pack_cache = root.join("git-pack-cache");
        // garage_meta and garage_data are (re)created per spawn attempt below
        // so a lost bind race cannot leave stale metadata behind.
        for path in [
            &postgres_data,
            &postgres_socket,
            &redis_data,
            &git_data,
            &git_pack_cache,
        ] {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        prepare_postgres_directories(
            postgres_root,
            &postgres_data,
            &postgres_socket,
            postgres_identity,
        )?;

        // Reserve every port up front: while a reservation is live no other
        // `reserve_port` call in this process can be handed the same number,
        // so a bind retry for one service cannot steal the port of another
        // service that has not started yet.
        let postgres_reservations = vec![port::reserve_port()?];
        let redis_reservations = vec![port::reserve_port()?];
        let garage_reservations = vec![port::reserve_port()?, port::reserve_port()?];
        let relay_reservations = vec![
            port::reserve_port()?,
            port::reserve_port()?,
            port::reserve_port()?,
        ];

        let mut initdb = Command::new("initdb");
        initdb.arg("-D").arg(&postgres_data).args([
            "-A",
            "trust",
            "--no-locale",
            "--encoding=UTF8",
            "--username",
            POSTGRES_SUPERUSER,
            // Test-only database: don't fsync the fresh cluster files.
            "--no-sync",
        ]);
        set_process_identity(&mut initdb, postgres_identity);
        run_checked("initdb", &mut initdb)?;

        let (mut postgres, postgres_ports) = spawn_with_bind_retry(
            "Postgres",
            root.join("postgres.log"),
            postgres_reservations,
            ReadyCheck::Tcp(&[0]),
            |ports| {
                let mut command = Command::new("postgres");
                command
                    .arg("-D")
                    .arg(&postgres_data)
                    .args(["-h", "127.0.0.1", "-p"])
                    .arg(ports[0].to_string())
                    .arg("-k")
                    .arg(&postgres_socket)
                    // Test-only database: trade crash durability for speed on
                    // cold, I/O-constrained CI runners.
                    .args(["-c", "fsync=off"])
                    .args(["-c", "synchronous_commit=off"])
                    .args(["-c", "full_page_writes=off"]);
                set_process_identity(&mut command, postgres_identity);
                Ok(command)
            },
        )
        .await?;
        let postgres_port_number = postgres_ports[0];
        wait_for_command(&mut postgres, || {
            Command::new("pg_isready")
                .args(["-h", "127.0.0.1", "-p"])
                .arg(postgres_port_number.to_string())
                .args(["-U", POSTGRES_SUPERUSER])
                .output()
        })
        .await?;
        run_checked(
            "createuser",
            Command::new("createuser")
                .args(["-h", "127.0.0.1", "-p"])
                .arg(postgres_port_number.to_string())
                .args(["-U", POSTGRES_SUPERUSER])
                .arg("buzz"),
        )?;
        run_checked(
            "createdb",
            Command::new("createdb")
                .args(["-h", "127.0.0.1", "-p"])
                .arg(postgres_port_number.to_string())
                .args(["-U", POSTGRES_SUPERUSER])
                .args(["-O", "buzz"])
                .arg("buzz"),
        )?;

        let (redis, redis_ports) = spawn_with_bind_retry(
            "Redis",
            root.join("redis.log"),
            redis_reservations,
            ReadyCheck::Tcp(&[0]),
            |ports| {
                let mut command = Command::new("redis-server");
                command
                    .args(["--bind", "127.0.0.1", "--port"])
                    .arg(ports[0].to_string())
                    .args(["--save", "", "--appendonly", "no", "--dir"])
                    .arg(&redis_data);
                Ok(command)
            },
        )
        .await?;
        let redis_port_number = redis_ports[0];

        let garage_config = root.join("garage.toml");
        let (garage, garage_ports) = spawn_with_bind_retry(
            "Garage",
            root.join("garage.log"),
            garage_reservations,
            ReadyCheck::Tcp(&[0, 1]),
            |ports| {
                let garage_s3_port_number = ports[0];
                let garage_rpc_port_number = ports[1];
                // Recreate the Garage state dirs on every attempt so a lost
                // bind race cannot leave metadata referencing a stale
                // rpc_public_addr behind.
                for path in [&garage_meta, &garage_data] {
                    if path.exists() {
                        fs::remove_dir_all(path)
                            .with_context(|| format!("failed to clear {}", path.display()))?;
                    }
                    fs::create_dir_all(path)
                        .with_context(|| format!("failed to create {}", path.display()))?;
                }
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
                let mut command = Command::new("garage");
                command.arg("-c").arg(&garage_config).arg("server");
                Ok(command)
            },
        )
        .await?;
        let garage_s3_port_number = garage_ports[0];

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

        let owner_pubkey = owner.public_key();
        let owner_secret = owner.secret_key().to_secret_hex();
        let database_url = format!("postgres://buzz@127.0.0.1:{postgres_port_number}/buzz");
        let redis_url = format!("redis://127.0.0.1:{redis_port_number}");
        let s3_endpoint = format!("http://127.0.0.1:{garage_s3_port_number}");

        let (relay, relay_ports) = spawn_with_bind_retry(
            "Buzz relay",
            root.join("buzz-relay.log"),
            relay_reservations,
            ReadyCheck::Http {
                port_index: 1,
                path: "/_readiness",
            },
            |ports| {
                let relay_port_number = ports[0];
                let health_port_number = ports[1];
                let metrics_port_number = ports[2];
                let http_url = format!("http://127.0.0.1:{relay_port_number}");
                let relay_url = format!("ws://127.0.0.1:{relay_port_number}");
                let mut command = Command::new(&buzz_relay);
                command
                    .current_dir(root)
                    .env("BUZZ_BIND_ADDR", format!("127.0.0.1:{relay_port_number}"))
                    .env("BUZZ_HEALTH_PORT", health_port_number.to_string())
                    .env("BUZZ_METRICS_PORT", metrics_port_number.to_string())
                    .env("DATABASE_URL", &database_url)
                    .env("REDIS_URL", &redis_url)
                    .env("BUZZ_AUTO_MIGRATE", "true")
                    .env("BUZZ_DB_POOL_SIZE", "8")
                    .env("BUZZ_REDIS_POOL_SIZE", "4")
                    .env("BUZZ_REQUIRE_AUTH_TOKEN", "false")
                    .env("BUZZ_REQUIRE_RELAY_MEMBERSHIP", "false")
                    .env("RELAY_URL", &relay_url)
                    .env("BUZZ_RELAY_PRIVATE_KEY", &owner_secret)
                    .env("RELAY_OWNER_PUBKEY", owner_pubkey.to_hex())
                    .env("BUZZ_S3_ENDPOINT", &s3_endpoint)
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
                Ok(command)
            },
        )
        .await?;
        let relay_port_number = relay_ports[0];
        let http_url = format!("http://127.0.0.1:{relay_port_number}");
        let relay_url = format!("ws://127.0.0.1:{relay_port_number}");

        Ok(Some(Self {
            http_url,
            relay_url,
            owner_pubkey,
            owner: owner.clone(),
            _relay: relay,
            _garage: garage,
            _redis: redis,
            _postgres: postgres,
            _postgres_temp_dir: postgres_temp_dir,
            _temp_dir: temp_dir,
        }))
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

/// Resolve the pinned test binary named by the `name` env var.
///
/// Returns `Ok(None)` — after printing why — when the binary is unavailable
/// and the `CI` environment variable is unset, so a plain `cargo test`
/// outside `nix develop` skips the Buzz coverage instead of failing. In CI
/// the binary is mandatory and a missing one is a hard error, so the
/// coverage cannot silently vanish from the pipeline.
fn binary_from_env(name: &str) -> Result<Option<PathBuf>> {
    let in_ci = env::var("CI").is_ok_and(|value| !value.is_empty());
    let Some(path) = env::var_os(name) else {
        if in_ci {
            bail!("{name} is not set; run the CI test step inside `nix develop`");
        }
        eprintln!(
            "skipping Buzz integration coverage: {name} is not set; \
             run the test inside `nix develop` to enable it"
        );
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if path.is_file() {
        return Ok(Some(path));
    }
    if in_ci {
        bail!("{name} points to missing binary {}", path.display());
    }
    eprintln!(
        "skipping Buzz integration coverage: {name} points to missing binary {}; \
         run the test inside `nix develop` to enable it",
        path.display()
    );
    Ok(None)
}

/// PostgreSQL refuses to initialize or serve a cluster as root. act's default
/// Linux job image runs commands as root even inside the microVM sandbox, so
/// use an existing unprivileged system account for only the PostgreSQL server
/// processes. Other fixture processes retain the caller's identity.
fn postgres_process_identity() -> Result<Option<ProcessIdentity>> {
    #[cfg(target_os = "linux")]
    {
        let effective_uid = fs::metadata("/proc/self")
            .context("failed to inspect the Buzz fixture process identity")?
            .uid();
        if effective_uid != 0 {
            return Ok(None);
        }

        let passwd = fs::read_to_string("/etc/passwd")
            .context("failed to read /etc/passwd for an unprivileged PostgreSQL identity")?;
        let identity = select_unprivileged_identity(&passwd).context(
            "the root Buzz fixture requires a non-root account in /etc/passwd to run PostgreSQL",
        )?;
        Ok(Some(identity))
    }

    #[cfg(not(target_os = "linux"))]
    Ok(None)
}

fn prepare_postgres_directories(
    root: &Path,
    data_dir: &Path,
    socket_dir: &Path,
    identity: Option<ProcessIdentity>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(identity) = identity {
        for path in [root, data_dir, socket_dir] {
            chown(path, Some(identity.uid), Some(identity.gid)).with_context(|| {
                format!(
                    "failed to give PostgreSQL fixture identity {}:{} ownership of {}",
                    identity.uid,
                    identity.gid,
                    path.display()
                )
            })?;
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (root, data_dir, socket_dir, identity);

    Ok(())
}

/// Prefer the conventional PostgreSQL or nobody accounts, then accept the
/// first non-root account so minimal CI images are not tied to one username.
fn select_unprivileged_identity(passwd: &str) -> Option<ProcessIdentity> {
    let mut nobody = None;
    let mut fallback = None;
    for line in passwd.lines() {
        let Some((name, identity)) = parse_passwd_identity(line) else {
            continue;
        };
        if identity.uid == 0 {
            continue;
        }
        if name == "postgres" {
            return Some(identity);
        }
        if name == "nobody" {
            nobody = Some(identity);
        } else if fallback.is_none() {
            fallback = Some(identity);
        }
    }
    nobody.or(fallback)
}

fn parse_passwd_identity(line: &str) -> Option<(&str, ProcessIdentity)> {
    let mut fields = line.split(':');
    let name = fields.next()?;
    fields.next()?; // password placeholder
    let uid = fields.next()?.parse::<u32>().ok()?;
    let gid = fields.next()?.parse::<u32>().ok()?;
    Some((name, ProcessIdentity { uid, gid }))
}

fn set_process_identity(command: &mut Command, identity: Option<ProcessIdentity>) {
    #[cfg(target_os = "linux")]
    if let Some(identity) = identity {
        command.gid(identity.gid).uid(identity.uid);
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (command, identity);
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

/// Which readiness signal gates a [`spawn_with_bind_retry`] attempt.
#[derive(Clone, Copy)]
enum ReadyCheck {
    /// Wait for a TCP accept on the ports at these indices, in order.
    Tcp(&'static [usize]),
    /// Wait for an HTTP 200 from `path` on the port at `port_index`.
    Http {
        port_index: usize,
        path: &'static str,
    },
}

/// Failure mode of a readiness wait. `EarlyExit` — the subprocess died
/// before becoming ready, the signature of a lost port-bind race and the
/// retry-eligible case — versus anything else. Mirrors `StartFailure` in
/// `grasp.rs`.
enum WaitFailure {
    EarlyExit {
        name: &'static str,
        status: ExitStatus,
        log: String,
    },
    Other(anyhow::Error),
}

impl From<WaitFailure> for anyhow::Error {
    fn from(value: WaitFailure) -> Self {
        match value {
            WaitFailure::EarlyExit { name, status, log } => {
                anyhow::anyhow!("{name} exited before becoming ready ({status})\n{log}")
            }
            WaitFailure::Other(error) => error,
        }
    }
}

/// Spawn a fixture subprocess that binds its own ports, retrying with fresh
/// reservations when it exits before becoming ready.
///
/// Same guard as `grasp.rs` and `relay.rs`: releasing a [`PortReservation`]
/// immediately before `spawn` leaves a microsecond-scale TOCTOU window in
/// which something outside this process can steal the port. Losing that race
/// makes the subprocess exit before its readiness probe passes; on that
/// signature we retry with freshly reserved ports, up to
/// [`MAX_BIND_ATTEMPTS`] times, instead of failing the test outright.
///
/// `build` receives the port numbers for the current attempt (in reservation
/// order) and must construct the subprocess `Command`, regenerating any
/// port-dependent state (config files, wiped data dirs) so every attempt
/// starts clean.
async fn spawn_with_bind_retry(
    name: &'static str,
    log_path: PathBuf,
    mut reservations: Vec<PortReservation>,
    ready: ReadyCheck,
    mut build: impl FnMut(&[u16]) -> Result<Command>,
) -> Result<(ManagedProcess, Vec<u16>)> {
    for attempt in 1..=MAX_BIND_ATTEMPTS {
        let ports: Vec<u16> = reservations.iter().map(PortReservation::port).collect();
        let mut command = build(&ports)?;
        // Release immediately before spawn to keep the TOCTOU window as
        // small as possible.
        for reservation in reservations.drain(..) {
            let _ = reservation.release();
        }
        let mut process = ManagedProcess::spawn(name, &mut command, log_path.clone())?;
        let readiness = match ready {
            ReadyCheck::Tcp(indices) => {
                let mut result = Ok(());
                for &index in indices {
                    result = wait_for_tcp(&mut process, ports[index]).await;
                    if result.is_err() {
                        break;
                    }
                }
                result
            }
            ReadyCheck::Http { port_index, path } => {
                wait_for_http_ready(&mut process, ports[port_index], path).await
            }
        };
        match readiness {
            Ok(()) => return Ok((process, ports)),
            Err(WaitFailure::EarlyExit { status, log, .. }) if attempt < MAX_BIND_ATTEMPTS => {
                eprintln!(
                    "[test_harness] {name} exited early on attempt \
                     {attempt}/{MAX_BIND_ATTEMPTS} (status: {status}); likely a \
                     port-bind race — retrying with fresh ports\n{log}"
                );
                reservations = ports
                    .iter()
                    .map(|_| port::reserve_port())
                    .collect::<Result<Vec<_>>>()
                    .with_context(|| {
                        format!("failed to reserve replacement ports after {name} early exit")
                    })?;
            }
            Err(failure) => return Err(anyhow::Error::from(failure)),
        }
    }
    unreachable!("MAX_BIND_ATTEMPTS loop terminated without returning")
}

async fn wait_for_tcp(
    process: &mut ManagedProcess,
    port: u16,
) -> std::result::Result<(), WaitFailure> {
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
            return Err(WaitFailure::Other(anyhow::anyhow!(
                "{} did not listen on 127.0.0.1:{port} within {READY_TIMEOUT:?}\n{}",
                process.name,
                process.log()
            )));
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

async fn wait_for_http_ready(
    process: &mut ManagedProcess,
    port: u16,
    path: &str,
) -> std::result::Result<(), WaitFailure> {
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
            return Err(WaitFailure::Other(anyhow::anyhow!(
                "{} did not pass HTTP readiness within {READY_TIMEOUT:?}\n{}",
                process.name,
                process.log()
            )));
        }
        sleep(READY_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_identity_is_preferred() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n\
                      nobody:x:65534:65534:nobody:/:/sbin/nologin\n\
                      postgres:x:71:71:postgres:/:/sbin/nologin\n";

        assert_eq!(
            select_unprivileged_identity(passwd),
            Some(ProcessIdentity { uid: 71, gid: 71 })
        );
    }

    #[test]
    fn nobody_is_preferred_to_an_arbitrary_non_root_account() {
        let passwd = "malformed\n\
                      root:x:0:0:root:/root:/bin/sh\n\
                      runner:x:1000:1000:runner:/home/runner:/bin/sh\n\
                      nobody:x:65534:65534:nobody:/:/sbin/nologin\n";

        assert_eq!(
            select_unprivileged_identity(passwd),
            Some(ProcessIdentity {
                uid: 65534,
                gid: 65534
            })
        );
    }

    #[test]
    fn root_is_never_selected() {
        assert_eq!(
            select_unprivileged_identity("root:x:0:0:root:/root:/bin/sh\n"),
            None
        );
    }
}
