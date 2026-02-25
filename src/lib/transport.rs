// TEMPORARY: Delete this entire file when async-wsocket merges Happy Eyeballs
// support and the rust-nostr version we use includes it.
// Upstream PR: https://github.com/shadowylab/async-wsocket/pull/42
//
// When removing this file, also:
//   1. Remove `pub mod transport;` from src/lib/mod.rs
//   2. Remove `.websocket_transport(HappyEyeballsTransport)` calls from
//      src/lib/client.rs
//   3. Remove the `use crate::transport::HappyEyeballsTransport;` import from
//      src/lib/client.rs
//   4. Remove `async-wsocket` and `tokio-tungstenite` from Cargo.toml direct
//      dependencies
//
// Interim Happy Eyeballs (RFC 8305) WebSocket transport for nostr-sdk.
//
// The default async-wsocket transport uses
// `tokio::net::TcpStream::connect(host:port)`, which resolves DNS and tries
// addresses sequentially. If IPv6 is returned first but is broken (hangs), the
// entire connection timeout is consumed before IPv4 is attempted.
//
// This module implements a custom `WebSocketTransport` that, for direct
// connections, resolves DNS and races IPv6/IPv4 with a 250ms head start for
// IPv6 (per RFC 8305).

use std::{
    fmt,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use async_wsocket::{ConnectionMode, Message, WebSocket};
use futures::{Sink, SinkExt, TryStreamExt, stream::StreamExt};
use nostr::{Url, util::BoxedFuture};
use nostr_relay_pool::transport::{
    error::TransportError,
    websocket::{WebSocketSink, WebSocketStream, WebSocketTransport},
};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::Message as TungsteniteMessage;

/// Delay before starting IPv4 attempts when IPv6 addresses are available (RFC
/// 8305).
const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);

/// Custom WebSocket transport implementing Happy Eyeballs (RFC 8305) for
/// IPv6/IPv4 fallback.
#[derive(Debug, Clone, Copy, Default)]
pub struct HappyEyeballsTransport;

impl WebSocketTransport for HappyEyeballsTransport {
    fn support_ping(&self) -> bool {
        true
    }

    fn connect<'a>(
        &'a self,
        url: &'a Url,
        mode: &'a ConnectionMode,
        timeout: Duration,
    ) -> BoxedFuture<'a, Result<(WebSocketSink, WebSocketStream), TransportError>> {
        Box::pin(async move {
            match mode {
                ConnectionMode::Direct => connect_happy_eyeballs(url, timeout).await,
                // For proxy/tor modes, delegate to the default implementation.
                _ => connect_default(url, mode, timeout).await,
            }
        })
    }
}

/// Fallback to the default async-wsocket connection for non-direct modes.
async fn connect_default(
    url: &Url,
    mode: &ConnectionMode,
    timeout: Duration,
) -> Result<(WebSocketSink, WebSocketStream), TransportError> {
    let socket: WebSocket = WebSocket::connect(url, mode, timeout)
        .await
        .map_err(TransportError::backend)?;
    let (tx, rx) = socket.split();
    let sink: WebSocketSink = Box::new(DefaultSinkAdapter(tx)) as WebSocketSink;
    let stream: WebSocketStream = Box::pin(rx.map_err(TransportError::backend)) as WebSocketStream;
    Ok((sink, stream))
}

/// Connect using Happy Eyeballs: resolve DNS, race IPv6 (with 250ms head start)
/// against IPv4, then perform TLS + WebSocket handshake on the winning TCP
/// connection.
async fn connect_happy_eyeballs(
    url: &Url,
    timeout: Duration,
) -> Result<(WebSocketSink, WebSocketStream), TransportError> {
    tokio::time::timeout(timeout, connect_happy_eyeballs_inner(url))
        .await
        .map_err(|_| {
            TransportError::backend(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection timed out",
            ))
        })?
}

async fn connect_happy_eyeballs_inner(
    url: &Url,
) -> Result<(WebSocketSink, WebSocketStream), TransportError> {
    let host = url
        .host_str()
        .ok_or_else(|| TransportError::backend(IoError("missing host in URL")))?;

    let default_port = match url.scheme() {
        "wss" => 443,
        "ws" => 80,
        _ => 80,
    };
    let port = url.port().unwrap_or(default_port);

    // Resolve DNS
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(TransportError::backend)?
        .collect();

    if addrs.is_empty() {
        return Err(TransportError::backend(IoError(
            "DNS resolution returned no addresses",
        )));
    }

    // Separate into IPv6 and IPv4
    let mut ipv6_addrs: Vec<SocketAddr> = Vec::new();
    let mut ipv4_addrs: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        if addr.is_ipv6() {
            ipv6_addrs.push(addr);
        } else {
            ipv4_addrs.push(addr);
        }
    }

    // Happy Eyeballs: try to connect via TCP
    let tcp_stream = happy_eyeballs_tcp(&ipv6_addrs, &ipv4_addrs).await?;

    // Build the WebSocket request URI from the URL
    let request_uri = url.as_str().to_string();

    // Perform TLS (if wss) + WebSocket handshake
    let (ws_stream, _response) = tokio_tungstenite::client_async_tls(&request_uri, tcp_stream)
        .await
        .map_err(TransportError::backend)?;

    // Split into sink + stream
    let (native_sink, native_stream) = ws_stream.split();

    // Wrap the sink: async_wsocket::Message -> tungstenite::Message (From impl
    // exists)
    let sink: WebSocketSink = Box::new(NativeSinkAdapter(native_sink)) as WebSocketSink;

    // Wrap the stream: tungstenite::Message -> async_wsocket::Message (manual
    // conversion)
    let stream: WebSocketStream = Box::pin(native_stream.map(|result| {
        result
            .map(tungstenite_to_async_wsocket)
            .map_err(TransportError::backend)
    })) as WebSocketStream;

    Ok((sink, stream))
}

/// Happy Eyeballs TCP connection algorithm.
///
/// If both IPv6 and IPv4 addresses are available, starts IPv6 first. If IPv6
/// doesn't connect within 250ms, starts IPv4 in parallel. Returns whichever
/// connects first.
async fn happy_eyeballs_tcp(
    ipv6_addrs: &[SocketAddr],
    ipv4_addrs: &[SocketAddr],
) -> Result<TcpStream, TransportError> {
    match (ipv6_addrs.is_empty(), ipv4_addrs.is_empty()) {
        // Only IPv4
        (true, false) => try_connect_addrs(ipv4_addrs).await,
        // Only IPv6
        (false, true) => try_connect_addrs(ipv6_addrs).await,
        // Both available: race with head start for IPv6
        (false, false) => {
            // Start IPv6 attempt
            let ipv6_fut = try_connect_addrs(ipv6_addrs);
            tokio::pin!(ipv6_fut);

            // Give IPv6 a 250ms head start
            let delay = tokio::time::sleep(HAPPY_EYEBALLS_DELAY);
            tokio::pin!(delay);

            // Wait for either IPv6 to succeed or the delay to expire
            tokio::select! {
                biased;
                result = &mut ipv6_fut => {
                    if let Ok(stream) = result {
                        return Ok(stream);
                    }
                    // IPv6 failed entirely, try IPv4
                    try_connect_addrs(ipv4_addrs).await
                }
                _ = &mut delay => {
                    // Delay expired, start IPv4 and race both
                    let ipv4_fut = try_connect_addrs(ipv4_addrs);
                    tokio::pin!(ipv4_fut);

                    tokio::select! {
                        biased;
                        result = &mut ipv6_fut => {
                            match result {
                                Ok(stream) => Ok(stream),
                                Err(_) => ipv4_fut.await,
                            }
                        }
                        result = &mut ipv4_fut => {
                            match result {
                                Ok(stream) => Ok(stream),
                                Err(_) => ipv6_fut.await,
                            }
                        }
                    }
                }
            }
        }
        // No addresses at all
        (true, true) => Err(TransportError::backend(IoError(
            "no addresses to connect to",
        ))),
    }
}

/// Try connecting to each address in sequence, returning the first successful
/// connection.
async fn try_connect_addrs(addrs: &[SocketAddr]) -> Result<TcpStream, TransportError> {
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(TransportError::backend(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no addresses to connect to",
        )
    })))
}

/// Convert a tungstenite::Message to an async_wsocket::Message.
///
/// Since `Message::from_native` is `pub(crate)` in async-wsocket, we replicate
/// the conversion here.
fn tungstenite_to_async_wsocket(msg: TungsteniteMessage) -> Message {
    match msg {
        TungsteniteMessage::Text(text) => Message::Text(text.to_string()),
        TungsteniteMessage::Binary(data) => Message::Binary(data.to_vec()),
        TungsteniteMessage::Ping(data) => Message::Ping(data.to_vec()),
        TungsteniteMessage::Pong(data) => Message::Pong(data.to_vec()),
        TungsteniteMessage::Close(frame) => {
            Message::Close(frame.map(|f| async_wsocket::message::CloseFrame {
                code: u16::from(f.code),
                reason: f.reason.to_string(),
            }))
        }
        // From tungstenite docs: "you're not going to get this value while reading"
        TungsteniteMessage::Frame(_) => unreachable!(),
    }
}

/// Sink adapter for the native tokio-tungstenite WebSocketStream.
///
/// Converts `async_wsocket::Message` to `tungstenite::Message` using the
/// existing `From` implementation, and maps errors to `TransportError`.
struct NativeSinkAdapter<S>(S);

impl<S> Sink<Message> for NativeSinkAdapter<S>
where
    S: Sink<TungsteniteMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    type Error = TransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_ready(cx)
            .map_err(TransportError::backend)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        // From<async_wsocket::Message> for tungstenite::Message exists
        let native_msg: TungsteniteMessage = item.into();
        Pin::new(&mut self.0)
            .start_send(native_msg)
            .map_err(TransportError::backend)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_flush(cx)
            .map_err(TransportError::backend)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_close(cx)
            .map_err(TransportError::backend)
    }
}

/// Sink adapter for the default async-wsocket path (proxy/tor modes).
///
/// Wraps the `SplitSink<WebSocket, Message>` and maps errors to
/// `TransportError`.
struct DefaultSinkAdapter(futures::stream::SplitSink<WebSocket, Message>);

impl Sink<Message> for DefaultSinkAdapter {
    type Error = TransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_ready_unpin(cx)
            .map_err(TransportError::backend)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        Pin::new(&mut self.0)
            .start_send_unpin(item)
            .map_err(TransportError::backend)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_flush_unpin(cx)
            .map_err(TransportError::backend)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_close_unpin(cx)
            .map_err(TransportError::backend)
    }
}

/// Simple error type for inline error messages.
#[derive(Debug)]
struct IoError(&'static str);

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for IoError {}
