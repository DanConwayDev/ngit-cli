//! Local relay fixture that owns its listener from allocation through shutdown.

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use nostr_sdk::prelude::LocalRelay;
use tokio::{
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

#[derive(Debug)]
pub(super) struct ListenerTask {
    task: JoinHandle<()>,
}

impl ListenerTask {
    pub(super) fn start(listener: TcpListener, relay: LocalRelay) -> Self {
        let server = relay;
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, addr) = accepted.expect("accept test relay connection");
                        let relay = server.clone();
                        connections.spawn(async move {
                            let upgrade = Arc::new(Mutex::new(None));
                            let requested_upgrade = upgrade.clone();
                            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                                let key = req.headers().get("sec-websocket-key").cloned();
                                let response = if let Some(key) = key {
                                    let accept = derive_accept_key(key.as_bytes());
                                    *requested_upgrade.lock().expect("upgrade lock") =
                                        Some(hyper::upgrade::on(req));
                                    Response::builder()
                                        .status(101)
                                        .header("connection", "upgrade")
                                        .header("upgrade", "websocket")
                                        .header("sec-websocket-accept", accept)
                                } else {
                                    Response::builder().status(400)
                                };
                                async move {
                                    Ok::<_, Infallible>(response.body(Full::new(Bytes::new())).unwrap())
                                }
                            });
                            let http = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service)
                                .with_upgrades();
                            if !matches!(tokio::time::timeout(Duration::from_secs(5), http).await, Ok(Ok(()))) {
                                return;
                            }
                            let upgrade = upgrade.lock().expect("upgrade lock").take();
                            if let Some(upgrade) = upgrade {
                                if let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(5), upgrade).await {
                                    let _ = relay.take_connection(TokioIo::new(stream), addr).await;
                                }
                            }
                        });
                    }
                    result = connections.join_next(), if !connections.is_empty() => {
                        result.expect("connection task").expect("connection task panicked");
                    }
                }
            }
        });
        Self { task }
    }
}

impl Drop for ListenerTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}
