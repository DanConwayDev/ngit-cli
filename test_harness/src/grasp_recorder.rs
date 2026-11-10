//! Recording GRASP proxy for proposal publication ordering tests.
//!
//! The proxy terminates only the client-facing WebSocket. Proposal `EVENT`
//! messages are forwarded to (or rejected before) a real GRASP service, while
//! relay `OK` messages and Smart HTTP receive-pack body bytes are recorded in
//! one per-proxy sequence. This proves acknowledgement ordering rather than
//! merely observing that an event and a ref both exist after the command.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use futures::{SinkExt, StreamExt};
use nostr_sdk::prelude::EventId;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Notify, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};

use crate::{
    grasp::GraspServer,
    nostr::{KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE},
    port,
};

/// Wire events observed by [`GraspRecorder`], in observation order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraspObservation {
    ProposalOffered(EventId),
    ProposalCreatedAt(EventId, u64),
    ProposalAccepted(EventId),
    ProposalRejected(EventId),
    ReceivePackBody,
    GitRequestRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraspRelayMode {
    Forward,
    RejectGit,
    Reject,
    RejectAfterGate,
    Unreachable,
}

/// A loopback GRASP endpoint backed by a real [`GraspServer`].
///
/// Dropping the recorder aborts its accept loop; dropping that loop's
/// [`JoinSet`] also cancels active connections without releasing its port.
pub struct GraspRecorder {
    url: String,
    observations: Arc<Mutex<Vec<GraspObservation>>>,
    offer_notify: Arc<Notify>,
    rejection_gate: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl GraspRecorder {
    pub async fn start(upstream: &GraspServer, reject_proposals: bool) -> Result<Self> {
        Self::start_with_mode(
            upstream,
            if reject_proposals {
                GraspRelayMode::Reject
            } else {
                GraspRelayMode::Forward
            },
        )
        .await
    }

    pub async fn start_with_mode(upstream: &GraspServer, mode: GraspRelayMode) -> Result<Self> {
        let reservation = port::reserve_port()?;
        let listener = reservation.into_std_listener();
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let url = format!("http://{}", listener.local_addr()?);
        let upstream_address = upstream
            .url()
            .strip_prefix("http://")
            .and_then(|url| url.split('/').next())
            .context("GRASP fixture URL is not loopback HTTP")?
            .to_string();
        let upstream_relay = upstream.relay_url();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&observations);
        let offer_notify = Arc::new(Notify::new());
        let (rejection_gate, _) = watch::channel(false);
        let offered = Arc::clone(&offer_notify);
        let gate = rejection_gate.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let recorded = Arc::clone(&recorded);
                                let offered = Arc::clone(&offered);
                                let rejection_gate = gate.subscribe();
                                let upstream_address = upstream_address.clone();
                                let upstream_relay = upstream_relay.clone();
                                connections.spawn(async move {
                                    if let Err(error) = serve_connection(
                                        stream,
                                        &upstream_address,
                                        &upstream_relay,
                                        mode,
                                        offered,
                                        rejection_gate,
                                        recorded,
                                    )
                                    .await
                                    {
                                        eprintln!("GraspRecorder connection ended: {error:#}");
                                    }
                                });
                            }
                            Err(error) => {
                                eprintln!("GraspRecorder accept failed: {error}");
                                break;
                            }
                        }
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        if let Err(error) = result {
                            eprintln!("GraspRecorder connection task failed: {error}");
                        }
                    }
                }
            }
        });
        Ok(Self {
            url,
            observations,
            offer_notify,
            rejection_gate,
            task,
        })
    }

    /// Base HTTP URL supplied as `--git-server`; ngit derives its paired
    /// WebSocket relay and GRASP-06 contributor endpoint from this URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn observations(&self) -> Vec<GraspObservation> {
        self.observations
            .lock()
            .expect("GraspRecorder observation lock")
            .clone()
    }

    /// Wait until an actual signed PR offer is observed, never a fixed delay.
    pub async fn wait_for_proposal_offer(&self) -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(crate::RelayTimeout::Normal.seconds()),
            async {
                loop {
                    let notified = self.offer_notify.notified();
                    if self
                        .observations()
                        .iter()
                        .any(|entry| matches!(entry, GraspObservation::ProposalOffered(_)))
                    {
                        return;
                    }
                    notified.await;
                }
            },
        )
        .await
        .context("timed out waiting for recorder to observe a signed PR offer")
    }

    pub fn release_rejection(&self) {
        // Retain the released state for connections which observed the offer
        // but have not registered their gate waiter yet.
        self.rejection_gate.send_replace(true);
    }
}

impl Drop for GraspRecorder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(
    mut client: TcpStream,
    upstream_address: &str,
    upstream_relay: &str,
    mode: GraspRelayMode,
    offer_notify: Arc<Notify>,
    rejection_gate: watch::Receiver<bool>,
    observations: Arc<Mutex<Vec<GraspObservation>>>,
) -> Result<()> {
    let prefix = read_http_head(&mut client).await?;
    let lowercase_head = String::from_utf8_lossy(&prefix).to_ascii_lowercase();
    if lowercase_head.starts_with("get ") && lowercase_head.contains("upgrade: websocket") {
        if mode == GraspRelayMode::Unreachable {
            bail!("paired relay is unreachable in this fixture");
        }
        serve_websocket(
            PrefixedStream::new(prefix, client),
            upstream_relay,
            mode,
            offer_notify,
            rejection_gate,
            observations,
        )
        .await
    } else if mode == GraspRelayMode::RejectGit {
        record(&observations, GraspObservation::GitRequestRejected);
        client.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        Ok(())
    } else {
        serve_http(client, prefix, upstream_address, observations).await
    }
}

async fn read_http_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0_u8; 8192];
        let read = stream.read(&mut buffer).await?;
        ensure!(read > 0, "client disconnected before its HTTP head");
        bytes.extend_from_slice(&buffer[..read]);
        if find_bytes(&bytes, b"\r\n\r\n").is_some() {
            return Ok(bytes);
        }
        ensure!(bytes.len() <= 64 * 1024, "unbounded HTTP request head");
    }
}

async fn serve_websocket(
    client: PrefixedStream,
    upstream_relay: &str,
    mode: GraspRelayMode,
    offer_notify: Arc<Notify>,
    rejection_gate: watch::Receiver<bool>,
    observations: Arc<Mutex<Vec<GraspObservation>>>,
) -> Result<()> {
    let mut client = accept_async(client).await?;
    let (mut upstream, _) = connect_async(upstream_relay).await?;
    let mut proposals = HashMap::new();

    loop {
        tokio::select! {
            message = client.next() => {
                let Some(message) = message else { break };
                let message = message?;
                if let Message::Text(text) = &message {
                    if let Some((event_id, kind)) = proposal_from_client_json(text) {
                        record(&observations, GraspObservation::ProposalOffered(event_id));
                        let payload: Value = serde_json::from_str(text)?;
                        record(&observations, GraspObservation::ProposalCreatedAt(
                            event_id, payload[1]["created_at"].as_u64().context("missing proposal timestamp")?,
                        ));
                        // `notify_one` stores a permit if the test has not
                        // started awaiting the just-recorded offer yet.
                        offer_notify.notify_one();
                        proposals.insert(event_id, kind);
                        if matches!(
                            mode,
                            GraspRelayMode::Reject | GraspRelayMode::RejectAfterGate
                        ) {
                            if mode == GraspRelayMode::RejectAfterGate {
                                let mut gate = rejection_gate.clone();
                                gate.wait_for(|released| *released)
                                    .await
                                    .context("recorder rejection gate closed before release")?;
                            }
                            record(&observations, GraspObservation::ProposalRejected(event_id));
                            let rejection = serde_json::json!([
                                "OK",
                                event_id.to_hex(),
                                false,
                                "restricted: proposal rejected by GraspRecorder"
                            ]);
                            client.send(Message::Text(rejection.to_string().into())).await?;
                            continue;
                        }
                    }
                }
                upstream.send(message).await?;
            }
            message = upstream.next() => {
                let Some(message) = message else { break };
                let message = message?;
                if let Message::Text(text) = &message {
                    if let Some((event_id, accepted)) = proposal_ok_from_relay_json(text, &proposals) {
                        record(
                            &observations,
                            if accepted {
                                GraspObservation::ProposalAccepted(event_id)
                            } else {
                                GraspObservation::ProposalRejected(event_id)
                            },
                        );
                    }
                }
                client.send(message).await?;
            }
        }
    }
    Ok(())
}

fn proposal_from_client_json(text: &str) -> Option<(EventId, u64)> {
    let json: Value = serde_json::from_str(text).ok()?;
    if json.get(0)?.as_str()? != "EVENT" {
        return None;
    }
    let kind = json.get(1)?.get("kind")?.as_u64()?;
    if kind != u64::from(KIND_PULL_REQUEST.as_u16())
        && kind != u64::from(KIND_PULL_REQUEST_UPDATE.as_u16())
    {
        return None;
    }
    let id = EventId::from_hex(json.get(1)?.get("id")?.as_str()?).ok()?;
    Some((id, kind))
}

fn proposal_ok_from_relay_json(
    text: &str,
    proposals: &HashMap<EventId, u64>,
) -> Option<(EventId, bool)> {
    let json: Value = serde_json::from_str(text).ok()?;
    if json.get(0)?.as_str()? != "OK" {
        return None;
    }
    let id = EventId::from_hex(json.get(1)?.as_str()?).ok()?;
    if !proposals.contains_key(&id) {
        return None;
    }
    Some((id, json.get(2)?.as_bool()?))
}

fn record(observations: &Mutex<Vec<GraspObservation>>, observation: GraspObservation) {
    observations
        .lock()
        .expect("GraspRecorder observation lock")
        .push(observation);
}

async fn serve_http(
    client: TcpStream,
    prefix: Vec<u8>,
    upstream_address: &str,
    observations: Arc<Mutex<Vec<GraspObservation>>>,
) -> Result<()> {
    let upstream = TcpStream::connect(upstream_address).await?;
    let (mut client_read, mut client_write) = client.into_split();
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let mut detector = ReceivePackDetector::default();
    detector.observe(&prefix, &observations);
    upstream_write.write_all(&prefix).await?;

    let upload_observations = Arc::clone(&observations);
    let upload = async move {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = client_read.read(&mut buffer).await?;
            if read == 0 {
                upstream_write.shutdown().await?;
                return Result::<()>::Ok(());
            }
            detector.observe(&buffer[..read], &upload_observations);
            upstream_write.write_all(&buffer[..read]).await?;
        }
    };
    let download = async move {
        tokio::io::copy(&mut upstream_read, &mut client_write).await?;
        Result::<()>::Ok(())
    };
    let (upload, download) = tokio::join!(upload, download);
    upload?;
    download?;
    Ok(())
}

#[derive(Default)]
struct ReceivePackDetector {
    buffered: Vec<u8>,
    awaiting_body: bool,
    recorded: bool,
}

impl ReceivePackDetector {
    fn observe(&mut self, bytes: &[u8], observations: &Mutex<Vec<GraspObservation>>) {
        if self.recorded {
            return;
        }
        if self.awaiting_body {
            if !bytes.is_empty() {
                self.record(observations);
            }
            return;
        }
        self.buffered.extend_from_slice(bytes);
        loop {
            let Some(post) = find_bytes(&self.buffered, b"POST ") else {
                let retain = self.buffered.len().min(4);
                self.buffered = self.buffered[self.buffered.len() - retain..].to_vec();
                return;
            };
            self.buffered = self.buffered[post..].to_vec();
            let Some(head_end) = find_bytes(&self.buffered, b"\r\n\r\n") else {
                return;
            };
            let is_receive_pack = String::from_utf8_lossy(&self.buffered[..head_end])
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("POST "))
                .and_then(|target| target.split_whitespace().next())
                .is_some_and(|target| target.ends_with("/git-receive-pack"));
            self.buffered = self.buffered[head_end + 4..].to_vec();
            if is_receive_pack {
                if self.buffered.is_empty() {
                    self.awaiting_body = true;
                } else {
                    self.record(observations);
                }
                return;
            }
        }
    }

    fn record(&mut self, observations: &Mutex<Vec<GraspObservation>>) {
        self.recorded = true;
        record(observations, GraspObservation::ReceivePackBody);
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Replays an already-consumed HTTP upgrade request before the remaining TCP
/// stream so tungstenite can perform its normal server handshake.
struct PrefixedStream {
    prefix: Vec<u8>,
    offset: usize,
    stream: TcpStream,
}

impl PrefixedStream {
    fn new(prefix: Vec<u8>, stream: TcpStream) -> Self {
        Self {
            prefix,
            offset: 0,
            stream,
        }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let count = (self.prefix.len() - self.offset).min(buffer.remaining());
            let end = self.offset + count;
            buffer.put_slice(&self.prefix[self.offset..end]);
            self.offset = end;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
