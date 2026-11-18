//! A signer reached over a local Unix socket speaking the draft NIP-5F
//! (nostr-protocol/nips#1862), such as nostrd or Amber Desktop.
//!
//! The signer holds the account keys; ngit only asks it for signatures and
//! NIP-44 ciphertexts. Npub selection prefers the socket; socket profiles pin
//! its backend and identity. With no login, use the daemon's active account.
//!
//! The socket is `nostr.signer-socket` from Git config, or
//! `~/.local/share/nostr/signer.sock`, where NIP-5F clients look. Setting
//! `nostr.signer-socket` to `false` turns this off.

use std::{path::PathBuf, time::Duration};

use anyhow::{Result, anyhow};
use nostr::prelude::PublicKey;
use tokio::time::timeout;
#[cfg(not(unix))]
pub use unsupported::SocketSigner;
#[cfg(unix)]
use {
    crate::git::{RepoActions, get_git_config_item_global},
    anyhow::{Context, bail},
    nostr::prelude::{Event, UnsignedEvent},
    serde_json::{Value, json},
    std::{
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    },
    tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{
            UnixStream,
            unix::{OwnedReadHalf, OwnedWriteHalf},
        },
        sync::Mutex,
    },
};

use crate::git::Repo;
// Compile the non-Unix implementation in Unix tests as well.
#[cfg(any(not(unix), test))]
#[cfg_attr(unix, allow(dead_code))]
mod unsupported;

/// How long to wait for the socket to accept and greet.
#[cfg(unix)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for an answer, including signer approval.
#[cfg(unix)]
const CALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// NIP-5F's account-not-found error code.
const ERROR_NOT_FOUND: i64 = 4;

pub const LOGOUT_MESSAGE: &str = "This account is supplied by your local signer; ngit cannot log it out. Manage it in your signer application, or disable socket signing with `git config --global nostr.signer-socket false` (a repository override takes precedence).";

/// Test processes may use an explicit isolated socket, never the default.
#[cfg(unix)]
pub fn socket_path(git_repo: &Option<&Repo>) -> Result<Option<PathBuf>> {
    let configured = match git_repo {
        Some(repo) => repo.get_git_config_item("nostr.signer-socket", None),
        None => get_git_config_item_global(git_repo, "nostr.signer-socket"),
    }?;
    let path = match configured.as_deref().map(str::trim) {
        Some("false" | "no" | "off" | "0" | "") => return Ok(None),
        Some(path) => expand_home(path),
        None if std::env::var("NGITTEST").is_ok() => return Ok(None),
        None => match default_socket() {
            Some(path) => path,
            None => return Ok(None),
        },
    };
    Ok(path.exists().then_some(path))
}

/// Socket discovery is disabled on platforms without the Unix transport.
#[cfg(not(unix))]
pub fn socket_path(_git_repo: &Option<&Repo>) -> Result<Option<PathBuf>> {
    Ok(None)
}

/// Live signing uses the transport's normal deadlines. Inventory shares a
/// bounded snapshot, including failures, for the lifetime of one command.
#[derive(Default)]
pub struct SocketProbe {
    snapshot: Option<ProbeSnapshot>,
}

type ProbeResult = Result<PublicKey, (bool, String)>;

struct ProbeSnapshot {
    remaining: Duration,
    accounts: std::collections::HashMap<(PathBuf, Option<PublicKey>), ProbeResult>,
}

impl SocketProbe {
    pub fn inventory() -> Self {
        Self {
            snapshot: Some(ProbeSnapshot {
                remaining: Duration::from_secs(2),
                accounts: std::collections::HashMap::new(),
            }),
        }
    }

    pub fn is_inventory(&self) -> bool {
        self.snapshot.is_some()
    }

    pub async fn public_key(
        &mut self,
        path: PathBuf,
        account: Option<PublicKey>,
    ) -> Result<PublicKey> {
        let Some(snapshot) = &mut self.snapshot else {
            return Ok(SocketSigner::connect(path, account).await?.public_key());
        };
        let key = (path.clone(), account);
        if !snapshot.accounts.contains_key(&key) {
            let started = std::time::Instant::now();
            let result = if snapshot.remaining.is_zero() {
                Err(anyhow!("local signer inventory probe budget exhausted"))
            } else {
                match timeout(
                    snapshot.remaining,
                    SocketSigner::connect(path.clone(), account),
                )
                .await
                {
                    Ok(result) => result.map(|signer| signer.public_key()),
                    Err(_) => Err(anyhow!("local signer inventory probe budget exhausted")),
                }
            };
            snapshot.remaining = snapshot.remaining.saturating_sub(started.elapsed());
            let result = result.map_err(|error| (is_not_found(&error), format!("{error:#}")));
            // Discovering the active account also resolves that exact identity.
            if let Ok(public_key) = &result {
                snapshot
                    .accounts
                    .insert((path, Some(*public_key)), Ok(*public_key));
            }
            snapshot.accounts.insert(key.clone(), result);
        }
        match &snapshot.accounts[&key] {
            Ok(public_key) => Ok(*public_key),
            Err((true, message)) => Err(SignerSocketError {
                code: ERROR_NOT_FOUND,
                message: message.clone(),
            }
            .into()),
            Err((false, message)) => Err(anyhow!(message.clone())),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct SocketStatus {
    pub selected: bool,
    pub active: bool,
    pub state: &'static str,
    pub npub: Option<String>,
    pub error: Option<String>,
}

/// Account inventory must not wait for approval from an external application.
pub async fn status(git_repo: &Option<&Repo>) -> Result<SocketStatus> {
    status_with_probe(git_repo, &mut SocketProbe::inventory()).await
}

pub async fn status_with_probe(
    git_repo: &Option<&Repo>,
    probe: &mut SocketProbe,
) -> Result<SocketStatus> {
    use nostr::prelude::ToBech32;

    use crate::login::existing::SocketSelection;
    // Invalid saved selections are reported by account inventory. They must
    // never turn an optional socket availability probe into active fallback.
    let selection =
        crate::login::existing::socket_selection(git_repo).unwrap_or(SocketSelection::OtherBackend);
    let account = match selection {
        SocketSelection::Prefer(account) | SocketSelection::Required(account) => Some(account),
        SocketSelection::Fallback | SocketSelection::OtherBackend => None,
    };
    let mut status = SocketStatus {
        selected: matches!(selection, SocketSelection::Required(_)),
        active: false,
        state: "unavailable",
        npub: None,
        error: None,
    };
    let Some(path) = socket_path(git_repo)? else {
        return Ok(status);
    };
    match probe.public_key(path, account).await {
        Ok(public_key) => {
            status.state = "available";
            status.active = selection != SocketSelection::OtherBackend;
            status.selected = status.active && account.is_some();
            status.npub = Some(public_key.to_bech32()?);
        }
        Err(error) => status.error = Some(format!("{error:#}")),
    }
    Ok(status)
}

impl SocketStatus {
    pub fn message(&self) -> String {
        if self.active {
            format!(
                "Using local socket signer ({}). {}",
                self.npub.as_deref().unwrap_or("unknown account"),
                if self.selected {
                    "The selected identity is using the socket backend."
                } else {
                    "No ngit login is selected; this is the fallback account."
                }
            )
        } else if self.state == "available" {
            format!(
                "Local socket signer available ({}); use --signer <npub> to request a socket account for one command.",
                self.npub.as_deref().unwrap_or("unknown account")
            )
        } else {
            format!("Local socket signer unavailable{}. {}", if self.selected { " (selected)" } else { "" },
                self.error.as_deref().unwrap_or("Socket signing is disabled or no socket was found; check nostr.signer-socket and your signer application."))
        }
    }
}

#[cfg(unix)]
fn default_socket() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".local/share/nostr/signer.sock"))
}

#[cfg(unix)]
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(unix)]
struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

#[cfg(unix)]
impl Connection {
    async fn open(path: &Path) -> Result<Self> {
        let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
            .await
            .context("timed out connecting to the signer socket")?
            .with_context(|| format!("failed to connect to the signer at {}", path.display()))?;
        let (read, writer) = stream.into_split();
        let mut connection = Self {
            reader: BufReader::new(read),
            writer,
        };
        // The signer greets first, naming itself and its methods.
        let greeting = timeout(CONNECT_TIMEOUT, connection.read_message())
            .await
            .context("timed out waiting for the signer to greet")??;
        if greeting.get("supported_methods").is_none() {
            bail!("the signer socket didn't greet as a NIP-5F signer");
        }
        connection.write(&json!({ "client": "ngit" })).await?;
        Ok(connection)
    }

    async fn write(&mut self, message: &Value) -> Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .context("failed to write to the signer socket")
    }

    async fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .await
                .context("failed to read from the signer socket")?;
            if read == 0 {
                bail!("the signer closed the connection");
            }
            if !line.trim().is_empty() {
                return serde_json::from_str(&line).context("the signer sent invalid JSON");
            }
        }
    }
}

/// An error the signer answered with.
#[derive(Debug)]
pub struct SignerSocketError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for SignerSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the signer refused: {} (error {})",
            self.message, self.code
        )
    }
}

impl std::error::Error for SignerSocketError {}

/// A connection to the signer that comes back if it drops.
#[cfg(unix)]
struct Channel {
    path: PathBuf,
    connection: Mutex<Option<Connection>>,
    next_id: AtomicU64,
}

/// One account in a socket signer. Every request names the account, so
/// switching the signer's active account mid-command changes nothing here.
#[cfg(unix)]
pub struct SocketSigner {
    channel: Channel,
    account: PublicKey,
}

#[cfg(unix)]
impl SocketSigner {
    /// Connect, and pick `account`, or the signer's active account when
    /// `None`. Fails when the signer doesn't hold that account, or has no
    /// active one.
    pub async fn connect(path: PathBuf, account: Option<PublicKey>) -> Result<Self> {
        let connection = Connection::open(&path).await?;
        let channel = Channel {
            path,
            connection: Mutex::new(Some(connection)),
            next_id: AtomicU64::new(1),
        };
        let params = match account {
            Some(account) => vec![json!(account.to_hex())],
            None => vec![],
        };
        let result = channel.call("get_public_key", params).await?;
        let public_key = result
            .as_str()
            .and_then(|hex| PublicKey::from_hex(hex).ok())
            .context("the signer returned an invalid public key")?;
        if account.is_some_and(|account| account != public_key) {
            bail!("the signer answered for a different account");
        }
        Ok(Self {
            channel,
            account: public_key,
        })
    }

    pub fn path(&self) -> &Path {
        &self.channel.path
    }

    pub fn public_key(&self) -> PublicKey {
        self.account
    }

    pub async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event> {
        if unsigned.pubkey != self.account {
            bail!("the event isn't for the signer's account");
        }
        let event = json!({
            "pubkey": unsigned.pubkey.to_hex(),
            "created_at": unsigned.created_at.as_secs(),
            "kind": unsigned.kind.as_u16(),
            "tags": unsigned.tags,
            "content": unsigned.content,
        });
        let result = self
            .channel
            .call("sign_event", vec![event, json!(self.account.to_hex())])
            .await?;
        // NIP-5F signers may answer with the event as an object or a string.
        let event = match result {
            Value::String(json) => Event::from_json(json),
            other => Event::from_json(other.to_string()),
        };
        event.context("the signer returned an invalid event")
    }

    pub async fn nip44_encrypt(&self, public_key: &PublicKey, content: &str) -> Result<String> {
        self.string_call("nip44_encrypt", public_key, content).await
    }

    pub async fn nip44_decrypt(&self, public_key: &PublicKey, payload: &str) -> Result<String> {
        self.string_call("nip44_decrypt", public_key, payload).await
    }

    async fn string_call(
        &self,
        method: &str,
        public_key: &PublicKey,
        text: &str,
    ) -> Result<String> {
        let params = vec![
            json!(public_key.to_hex()),
            json!(text),
            json!(self.account.to_hex()),
        ];
        match self.channel.call(method, params).await? {
            Value::String(text) => Ok(text),
            _ => bail!("the signer's {method} returned something other than a string"),
        }
    }
}

#[cfg(unix)]
impl Channel {
    /// Send one request and read its answer, reconnecting once if the
    /// connection went away (the signer restarted, say) before it was sent.
    async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let mut guard = self.connection.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let request = json!({ "id": id, "method": method, "params": params });
        let exchange = async {
            let mut retried = false;
            loop {
                if guard.is_none() {
                    *guard = Some(Connection::open(&self.path).await?);
                }
                let connection = guard.as_mut().expect("connected above");
                if let Err(error) = connection.write(&request).await {
                    *guard = None;
                    if retried {
                        return Err(error);
                    }
                    retried = true;
                    continue;
                }
                return read_response(connection, &id).await;
            }
        };
        // Include writes and reconnects: an unresponsive reader can fill the
        // socket buffer before ngit ever starts waiting for a response.
        let response = match timeout(CALL_TIMEOUT, exchange).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                *guard = None;
                return Err(error);
            }
            Err(_) => {
                *guard = None;
                bail!("the signer didn't answer {method} in time");
            }
        };
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            return Err(anyhow!(SignerSocketError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            }));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// The answer to request `id`, skipping event lines (which have no id).
#[cfg(unix)]
async fn read_response(connection: &mut Connection, id: &str) -> Result<Value> {
    loop {
        let message = connection.read_message().await?;
        match message.get("id") {
            Some(Value::String(got)) if got == id => return Ok(message),
            Some(Value::Number(got)) if got.to_string() == id => return Ok(message),
            _ => {}
        }
    }
}

/// Whether `error` is the signer saying it doesn't hold the account (or has
/// no active one).
pub fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<SignerSocketError>()
        .is_some_and(|error| error.code == ERROR_NOT_FOUND)
}

#[cfg(unix)]
impl std::fmt::Debug for SocketSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SocketSigner({}, {})",
            self.path().display(),
            self.account
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use nostr::prelude::{EventBuilder, FinalizeUnsignedEvent, Keys, Kind, SignEvent};
    use tokio::net::UnixListener;

    use super::*;

    #[test]
    fn internal_errors_preserve_the_daemon_explanation() {
        for message in ["database corruption", "locked"] {
            let error = SignerSocketError {
                code: 10,
                message: message.to_string(),
            };
            let rendered = error.to_string();
            assert!(rendered.contains(message));
            assert!(rendered.contains("10"));
            assert!(!rendered.contains("Unlock"));
            assert!(!is_not_found(&anyhow!(error)));
        }
    }

    /// A NIP-5F signer for one key, answering as nostrd does.
    async fn serve(keys: Keys) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signer.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let keys = keys.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    write
                        .write_all(b"{\"name\":\"test\",\"supported_methods\":[]}\n")
                        .await
                        .unwrap();
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.unwrap() > 0 {
                        let message: Value = serde_json::from_str(&line).unwrap();
                        line.clear();
                        let Some(method) = message.get("method").and_then(Value::as_str) else {
                            continue;
                        };
                        let params = message["params"].as_array().cloned().unwrap_or_default();
                        let account = params.last().and_then(Value::as_str);
                        let ours = keys.public_key().to_hex();
                        let response = if account.is_some_and(|a| a != ours && a.len() == 64) {
                            json!({"id": message["id"], "result": null, "error": {"code": 4, "message": "account not found"}})
                        } else {
                            let result = match method {
                                "get_public_key" => json!(ours),
                                "sign_event" => {
                                    let unsigned = UnsignedEvent::from_json(
                                        json!({
                                            "pubkey": params[0]["pubkey"],
                                            "created_at": params[0]["created_at"],
                                            "kind": params[0]["kind"],
                                            "tags": params[0]["tags"],
                                            "content": params[0]["content"],
                                        })
                                        .to_string(),
                                    )
                                    .unwrap();
                                    serde_json::to_value(keys.sign_event(unsigned).unwrap())
                                        .unwrap()
                                }
                                _ => Value::Null,
                            };
                            json!({"id": message["id"], "result": result, "error": null})
                        };
                        let mut out = response.to_string();
                        out.push('\n');
                        write.write_all(out.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        (dir, path)
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_request_write_times_out_and_discards_connection() -> Result<()> {
        use tokio::io::AsyncReadExt;

        // The peer stays open but never reads during the request.
        let (stream, mut peer) = UnixStream::pair()?;
        stream.writable().await?;
        let (read, writer) = stream.into_split();
        let channel = Channel {
            path: PathBuf::from("unused.sock"),
            connection: Mutex::new(Some(Connection {
                reader: BufReader::new(read),
                writer,
            })),
            next_id: AtomicU64::new(1),
        };
        let payload = "x".repeat(8 * 1024 * 1024);
        let result = timeout(
            CALL_TIMEOUT + Duration::from_secs(1),
            channel.call("sign_event", vec![json!(payload)]),
        )
        .await
        .context("request write escaped the channel deadline")?;
        assert!(result.unwrap_err().to_string().contains("in time"));
        assert!(channel.connection.lock().await.is_none());

        // EOF proves the timed-out connection was closed; a partial payload
        // proves the timeout occurred during writing, not response reading.
        let mut received = Vec::new();
        timeout(Duration::from_secs(1), peer.read_to_end(&mut received)).await??;
        assert!(!received.is_empty());
        assert!(received.len() < payload.len());
        Ok(())
    }

    #[tokio::test]
    async fn signs_with_the_active_account() -> Result<()> {
        let keys = Keys::generate();
        let (_dir, path) = serve(keys.clone()).await;
        let signer = SocketSigner::connect(path, None).await?;
        assert_eq!(signer.public_key(), keys.public_key());
        let unsigned = EventBuilder::new(Kind::TextNote, "hi").finalize_unsigned(keys.public_key());
        let id = unsigned.compute_id();
        let event = signer.sign_event(unsigned).await?;
        assert_eq!(event.id, id);
        event.verify()?;
        Ok(())
    }

    #[tokio::test]
    async fn an_account_it_does_not_hold_is_not_found() {
        let (_dir, path) = serve(Keys::generate()).await;
        let error = SocketSigner::connect(path, Some(Keys::generate().public_key()))
            .await
            .unwrap_err();
        assert!(is_not_found(&error));
    }
}
