//! A signer reached over a local Unix socket speaking the draft NIP-5F
//! (nostr-protocol/nips#1862), such as nostrd or Amber Desktop.
//!
//! The signer holds the account keys; ngit only asks it for signatures and
//! NIP-44 ciphertexts. When one is listening, ngit signs through it for any
//! account it holds, instead of reading a key or opening a bunker connection
//! of its own.
//!
//! The socket is `nostr.signer-socket` from Git config, or
//! `~/.local/share/nostr/signer.sock`, where NIP-5F clients look. Setting
//! `nostr.signer-socket` to `false` turns this off.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use nostr::prelude::{Event, PublicKey, UnsignedEvent};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Mutex,
    time::timeout,
};

use crate::git::{Repo, RepoActions, get_git_config_item_global};

/// How long to wait for the socket to accept and greet.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for an answer. A signer may one day ask its user to
/// approve a request, so this is generous.
const CALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// NIP-5F's error codes, as nostrd uses them.
const ERROR_NOT_FOUND: i64 = 4;
const ERROR_LOCKED: i64 = 10;

/// The socket to use, if it's turned on and something is there.
pub fn socket_path(git_repo: &Option<&Repo>) -> Option<PathBuf> {
    if std::env::var("NGITTEST").is_ok() {
        return None;
    }
    let configured = match git_repo {
        Some(repo) => repo.get_git_config_item("nostr.signer-socket", None),
        None => get_git_config_item_global(git_repo, "nostr.signer-socket"),
    }
    .ok()
    .flatten();
    let path = match configured.as_deref().map(str::trim) {
        Some("false" | "no" | "off" | "0" | "") => return None,
        Some(path) => expand_home(path),
        None => default_socket()?,
    };
    path.exists().then_some(path)
}

fn default_socket() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".local/share/nostr/signer.sock"))
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

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
        match self.code {
            ERROR_LOCKED => f.write_str("the local signer is locked.\nUnlock it and try again."),
            _ => write!(
                f,
                "the signer refused: {} (error {})",
                self.message, self.code
            ),
        }
    }
}

impl std::error::Error for SignerSocketError {}

/// A connection to the signer that comes back if it drops.
struct Channel {
    path: PathBuf,
    connection: Mutex<Option<Connection>>,
    next_id: AtomicU64,
}

/// One account in a socket signer. Every request names the account, so
/// switching the signer's active account mid-command changes nothing here.
pub struct SocketSigner {
    channel: Channel,
    account: PublicKey,
}

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

impl Channel {
    /// Send one request and read its answer, reconnecting once if the
    /// connection went away (the signer restarted, say) before it was sent.
    async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let mut guard = self.connection.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let request = json!({ "id": id, "method": method, "params": params });
        let mut retried = false;
        let response = loop {
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
            match timeout(CALL_TIMEOUT, read_response(connection, &id)).await {
                Ok(Ok(response)) => break response,
                Ok(Err(error)) => {
                    *guard = None;
                    return Err(error);
                }
                Err(_) => {
                    *guard = None;
                    bail!("the signer didn't answer {method} in time");
                }
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

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, FinalizeUnsignedEvent, Keys, Kind, SignEvent};
    use tokio::net::UnixListener;

    use super::*;

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
