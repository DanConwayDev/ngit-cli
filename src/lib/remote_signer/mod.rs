// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2025 Rust Nostr Developers
// Distributed under the MIT software license

//! Nostr Connect client

use std::{collections::HashMap, fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use async_utility::time;
use futures::stream::BoxStream;
use nostr::{
    nips::{
        nip04::AsyncNip04,
        nip44::{self, AsyncNip44},
        nip46::{
            NostrConnectEventBuilder, NostrConnectMessage, NostrConnectMethod, NostrConnectRequest,
            NostrConnectResponse, NostrConnectUri, ResponseResult,
        },
    },
    types::Url,
};
use nostr_connect::error::{Error, ErrorKind};
use nostr_sdk::prelude::*;
use tokio::sync::{OnceCell, broadcast};

/// Progress without sensitive message contents.
#[derive(Debug, Clone)]
pub enum ConnectProgress {
    /// Results of dispatching the response subscription to the relays.
    Listening {
        output: nostr_sdk::client::Output<SubscriptionId>,
    },
    /// A relay processed the response subscription (matching EOSE).
    Ready { relay_url: RelayUrl },
    /// The connection handshake succeeded.
    Paired,
    /// A request is about to be sent to the signer relays.
    Request {
        event_id: EventId,
        method: NostrConnectMethod,
    },
    /// The SDK finished sending the request, including each relay's outcome.
    Sent {
        event_id: EventId,
        output: nostr_sdk::client::SendEventOutput,
    },
    /// A matching final response was received.
    Response { event_id: EventId },
}

/// Nostr Connect Client
///
/// <https://github.com/nostr-protocol/nips/blob/master/46.md>
#[derive(Debug, Clone)]
pub struct NostrConnect {
    progress: broadcast::Sender<ConnectProgress>,
    uri: NostrConnectUri,
    client_keys: Keys,
    remote_signer_public_key: OnceCell<PublicKey>,
    user_public_key: OnceCell<PublicKey>,
    client: Client,
    timeout: Duration,
    opts: RelayOptions,
    auth_url_handler: Option<Arc<dyn AuthUrlHandler>>,
}

impl NostrConnect {
    /// Construct Nostr Connect client
    pub fn new(
        uri: NostrConnectUri,
        client_keys: Keys,
        timeout: Duration,
        opts: Option<RelayOptions>,
    ) -> Result<Self, Error> {
        // Check app keys
        if let NostrConnectUri::Client { public_key, .. } = &uri {
            if public_key != &client_keys.public_key() {
                return Err(Error::with_static_message(
                    ErrorKind::Invalid,
                    "public key not match app keys",
                ));
            }
        }

        Ok(Self {
            progress: broadcast::channel(64).0,
            uri,
            client_keys,
            // NOT set the remote_signer_public_key, also if bunker URI!
            // If you already set remote_signer_public_key, you'll need another field to know if
            // boostrap was already done. If the URI is bunker, the
            // remote_signer_public_key is set in the bootstrap method.
            remote_signer_public_key: OnceCell::new(),
            user_public_key: OnceCell::new(),
            client: Client::default(),
            timeout,
            opts: opts.unwrap_or_default(),
            auth_url_handler: None,
        })
    }

    /// Set an `auth_url` handler
    ///
    /// ```ignore
    /// use std::future::Future;
    /// use std::pin::Pin;
    /// use std::time::Duration;
    ///
    /// use nostr_connect::prelude::*;
    ///
    /// #[derive(Debug, Clone)]
    /// struct MyAuthUrlHandler;
    ///
    /// impl AuthUrlHandler for MyAuthUrlHandler {
    ///     fn on_auth_url(&self, auth_url: Url) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>> {
    ///         Box::pin(async move {
    ///             webbrowser::open(auth_url.as_str()).map_err(Error::other)?;
    ///             Ok(())
    ///         })
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let uri = NostrConnectUri::parse("bunker://79dff8f82963424e0bb02708a22e44b4980893e3a4be0fa3cb60a43b946764e3?relay=wss://relay.nsec.app")?;
    ///     let client_keys = Keys::generate();
    ///     let timeout = Duration::from_secs(60);
    ///
    ///     let mut connect = NostrConnect::new(uri, client_keys, timeout, None)?;
    ///
    ///     // Set auth_url handler
    ///     connect.auth_url_handler(MyAuthUrlHandler);
    ///
    ///     // ...
    ///
    ///     Ok(())
    /// }
    /// ```
    #[inline]
    pub fn auth_url_handler<T>(&mut self, handler: T)
    where
        T: IntoAuthUrlHandler,
    {
        self.auth_url_handler = Some(handler.into_auth_url_handler());
    }

    /// Subscribe before starting an operation to observe its progress.
    pub fn progress(&self) -> broadcast::Receiver<ConnectProgress> {
        self.progress.subscribe()
    }

    /// Get relays status
    pub async fn status(&self) -> HashMap<RelayUrl, RelayStatus> {
        let relays = self.client.relays().await;
        relays.into_iter().map(|(u, r)| (u, r.status())).collect()
    }

    async fn bootstrap(&self) -> Result<PublicKey, Error> {
        // Add relays
        for url in self.uri.relays().iter() {
            self.client.add_relay(url).opts(self.opts.clone()).await?;
        }

        // Connect to relays
        self.client.connect().await;

        // Subscribe
        let (notifications, subscription_id) = self.subscribe().await?;

        // Get remote signer public key
        let remote_signer_public_key: PublicKey = match &self.uri {
            NostrConnectUri::Bunker {
                remote_signer_public_key,
                ..
            } => *remote_signer_public_key,
            NostrConnectUri::Client { secret, .. } => {
                // For nostrconnect:// (client-initiated), the secret is required by NIP-46.
                get_remote_signer_public_key(
                    &self.client_keys,
                    secret,
                    notifications,
                    self.timeout,
                    &subscription_id,
                    &self.progress,
                )
                .await?
            }
        };

        // Send `connect` command if bunker URI
        if let NostrConnectUri::Bunker { secret, .. } = &self.uri {
            self.connect_bunker(remote_signer_public_key, secret)
                .await?;
        }

        let _ = self.progress.send(ConnectProgress::Paired);
        Ok(remote_signer_public_key)
    }

    async fn subscribe(
        &self,
    ) -> Result<(BoxStream<'_, ClientNotification>, SubscriptionId), Error> {
        let public_key: PublicKey = self.client_keys.public_key();

        let filter = Filter::new()
            .pubkey(public_key)
            .kind(Kind::NostrConnect)
            .limit(0);

        let notifications = self.client.notifications();

        // Subscribe
        let output = self.client.subscribe(filter).await?;
        let subscription_id = output.value.clone();
        let _ = self.progress.send(ConnectProgress::Listening { output });

        Ok((notifications, subscription_id))
    }

    /// Get client keys, used for communicating with the remote signer
    #[inline]
    pub fn local_keys(&self) -> &Keys {
        &self.client_keys
    }

    /// Get signer relays
    #[inline]
    pub fn relays(&self) -> &[RelayUrl] {
        self.uri.relays()
    }

    /// Get `bunker` URI
    pub async fn bunker_uri(&self) -> Result<NostrConnectUri, Error> {
        Ok(NostrConnectUri::Bunker {
            remote_signer_public_key: *self.remote_signer_public_key().await?,
            relays: self.relays().to_vec(),
            // Not use the secret. The secret is used only for the first connection.
            secret: None,
        })
    }

    /// Manually set the user public key
    ///
    /// Be cautious when using this method, as providing an incorrect
    /// [`PublicKey`] can lead to potential issues.
    ///
    /// According to [`NIP46`](https://github.com/nostr-protocol/nips/blob/926a51e72206dfa71b9950a0ce58b54a7422aad2/46.md) the user public key must be requested from the signer.
    /// Once you set the user public key with this method, it can't be longer
    /// changed ans you'll have to create a new client. This method is
    /// intended to be used only if you are certain that public key stored by
    /// the signer match your (stored) one.
    #[inline]
    pub fn non_secure_set_user_public_key(&self, user_public_key: PublicKey) -> Result<(), Error> {
        Ok(self.user_public_key.set(user_public_key)?)
    }

    #[inline]
    async fn remote_signer_public_key(&self) -> Result<&PublicKey, Error> {
        self.remote_signer_public_key
            .get_or_try_init(|| async { self.bootstrap().await })
            .await
    }

    #[inline]
    async fn send_request(&self, req: NostrConnectRequest) -> Result<ResponseResult, Error> {
        // Get remote signer public key
        let remote_signer_public_key: PublicKey = *self.remote_signer_public_key().await?;

        // Send request
        self.send_request_with_pk(req, remote_signer_public_key)
            .await
    }

    async fn send_request_with_pk(
        &self,
        req: NostrConnectRequest,
        remote_signer_public_key: PublicKey,
    ) -> Result<ResponseResult, Error> {
        let secret_key: &SecretKey = self.client_keys.secret_key();

        // Convert request to event
        let msg = NostrConnectMessage::request(&req);
        tracing::debug!("Sending '{msg}' NIP46 message");

        let req_id = msg.id().to_string();
        let event: Event = NostrConnectEventBuilder::new(remote_signer_public_key, msg)
            .finalize(&self.client_keys)?;

        let mut notifications = self.client.notifications();

        // Send request
        let request_event_id = event.id;
        let _ = self.progress.send(ConnectProgress::Request {
            event_id: event.id,
            method: req.method(),
        });
        let output = self.client.send_event(&event).await?;
        let _ = self.progress.send(ConnectProgress::Sent {
            event_id: event.id,
            output,
        });

        time::timeout(Some(self.timeout), async {
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    if event.kind == Kind::NostrConnect {
                        let msg: String =
                            nip44::decrypt(secret_key, &event.pubkey, event.content.as_str())?;
                        let msg: NostrConnectMessage = NostrConnectMessage::from_json(msg)?;

                        tracing::debug!("Received NIP46 message: '{msg}'");

                        if req_id == msg.id() && msg.is_response() {
                            let response: NostrConnectResponse = msg.to_response(req.method())?;

                            if response.is_auth_url() {
                                if let (Some(auth_url), Some(handler)) =
                                    (response.error, &self.auth_url_handler)
                                {
                                    match Url::parse(&auth_url) {
                                        Ok(url) => {
                                            if let Err(e) = handler.on_auth_url(url).await {
                                                tracing::error!(
                                                    "Impossible to handle `auth_url`: {e}"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!("Can't parse `auth_url`: {e}")
                                        }
                                    }
                                }
                            } else {
                                let _ = self.progress.send(ConnectProgress::Response {
                                    event_id: request_event_id,
                                });
                                if let Some(error) = response.error {
                                    return Err(Error::new(ErrorKind::Rejected, error));
                                }

                                if let Some(result) = response.result {
                                    return Ok(result);
                                }

                                break;
                            }
                        }
                    }
                }
            }

            Err(Error::simple(ErrorKind::Timeout))
        })
        .await
        .ok_or_else(|| Error::simple(ErrorKind::Timeout))?
    }

    /// Connect bunker
    async fn connect_bunker(
        &self,
        remote_signer_public_key: PublicKey,
        secret: &Option<String>,
    ) -> Result<(), Error> {
        let req = NostrConnectRequest::Connect {
            remote_signer_public_key,
            secret: secret.clone(),
        };
        let res: ResponseResult = self
            .send_request_with_pk(req, remote_signer_public_key)
            .await?;

        if is_valid_connect_response(&res, secret.as_deref()) {
            return Ok(());
        }

        Err(Error::new(ErrorKind::Invalid, res.to_string()))
    }

    async fn _get_public_key(&self) -> Result<&PublicKey, Error> {
        self.user_public_key
            .get_or_try_init(|| async {
                let res = self.send_request(NostrConnectRequest::GetPublicKey).await?;
                Ok(res.to_get_public_key()?)
            })
            .await
    }

    /// Sign an [UnsignedEvent]
    async fn _sign_event(&self, unsigned: UnsignedEvent) -> Result<Event, Error> {
        let req = NostrConnectRequest::SignEvent(unsigned);
        let res = self.send_request(req).await?;
        Ok(res.to_sign_event()?)
    }

    async fn _nip04_encrypt(
        &self,
        public_key: PublicKey,
        content: String,
    ) -> Result<String, Error> {
        let req = NostrConnectRequest::Nip04Encrypt {
            public_key,
            text: content,
        };
        let res = self.send_request(req).await?;
        Ok(res.to_nip04_encrypt()?)
    }

    async fn _nip04_decrypt(
        &self,
        public_key: PublicKey,
        ciphertext: String,
    ) -> Result<String, Error> {
        let req = NostrConnectRequest::Nip04Decrypt {
            public_key,
            ciphertext,
        };
        let res = self.send_request(req).await?;
        Ok(res.to_nip04_decrypt()?)
    }

    async fn _nip44_encrypt(
        &self,
        public_key: PublicKey,
        content: String,
    ) -> Result<String, Error> {
        let req = NostrConnectRequest::Nip44Encrypt {
            public_key,
            text: content,
        };
        let res = self.send_request(req).await?;
        Ok(res.to_nip44_encrypt()?)
    }

    async fn _nip44_decrypt(
        &self,
        public_key: PublicKey,
        payload: String,
    ) -> Result<String, Error> {
        let req = NostrConnectRequest::Nip44Decrypt {
            public_key,
            ciphertext: payload,
        };
        let res = self.send_request(req).await?;
        Ok(res.to_nip44_decrypt()?)
    }

    /// Completely shutdown
    pub async fn shutdown(self) {
        self.client.shutdown().await
    }
}

async fn get_remote_signer_public_key(
    client_keys: &Keys,
    expected_secret: &str,
    mut notifications: BoxStream<'_, ClientNotification>,
    timeout: Duration,
    subscription_id: &SubscriptionId,
    progress: &broadcast::Sender<ConnectProgress>,
) -> Result<PublicKey, Error> {
    time::timeout(Some(timeout), async {
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Message { relay_url, message } = &notification {
                if matches!(message.as_ref(), RelayMessage::EndOfStoredEvents(id) if id.as_ref() == subscription_id) {
                    let _ = progress.send(ConnectProgress::Ready { relay_url: relay_url.clone() });
                }
            }
            if let ClientNotification::Event { event, .. } = notification {
                if event.kind == Kind::NostrConnect {
                    // Decrypt content
                    let msg: String = match nip44::decrypt(
                        client_keys.secret_key(),
                        &event.pubkey,
                        event.content.as_str(),
                    ) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };

                    // Parse message
                    let msg: NostrConnectMessage = match NostrConnectMessage::from_json(msg) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };

                    // The Debug and Display implementations of NostrConnectMessage redact the sensitive data.
                    tracing::debug!("Received Nostr Connect message: '{msg}'");

                    // Check if it's a `connect` response.
                    //
                    // Per NIP-46, for nostrconnect:// (client-initiated) connections the signer
                    // sends a `connect` response whose `result` is the secret value from the URI
                    // (not "ack"). The client MUST validate the returned secret to prevent
                    // connection spoofing.
                    if let Ok(NostrConnectResponse {
                        result: Some(result),
                        error: None,
                    }) = msg.to_response(NostrConnectMethod::Connect)
                    {
                        if is_valid_connect_response(&result, Some(expected_secret)) {
                            return Ok(event.pubkey);
                        } else {
                            tracing::warn!(
                                "Received connect response with unexpected result; ignoring"
                            );
                        }
                    }
                }
            }
        }

        Err(Error::with_static_message(ErrorKind::NotFound, "signer public key not found"))
    })
    .await
    .ok_or_else(|| Error::simple(ErrorKind::Timeout))?
}

fn is_valid_connect_response(response: &ResponseResult, expected_secret: Option<&str>) -> bool {
    match &response {
        // Some signers (e.g. those following older interpretations) return "ack"
        ResponseResult::Ack => true,
        // Per current NIP-46 spec the signer returns the secret value
        ResponseResult::ConnectSecret(s) => match expected_secret {
            Some(expected_secret) => s == expected_secret,
            None => false,
        },
        _ => false,
    }
}

/// Nostr Connect auth_url handler
pub trait AuthUrlHandler: fmt::Debug + Send + Sync {
    /// Handle `auth_url` message
    fn on_auth_url(
        &self,
        auth_url: Url,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>>;
}

#[doc(hidden)]
pub trait IntoAuthUrlHandler {
    fn into_auth_url_handler(self) -> Arc<dyn AuthUrlHandler>;
}

impl<T> IntoAuthUrlHandler for T
where
    T: AuthUrlHandler + 'static,
{
    fn into_auth_url_handler(self) -> Arc<dyn AuthUrlHandler> {
        Arc::new(self)
    }
}

impl AsyncGetPublicKey for NostrConnect {
    type Error = Error;

    #[inline]
    fn get_public_key_async(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<PublicKey, Self::Error>> + Send + '_>> {
        Box::pin(async move { self._get_public_key().await.copied() })
    }
}

impl AsyncSignEvent for NostrConnect {
    type Error = Error;

    #[inline]
    fn sign_event_async(
        &self,
        unsigned: UnsignedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Event, Self::Error>> + Send + '_>> {
        Box::pin(async move { self._sign_event(unsigned).await })
    }
}

impl AsyncNip04 for NostrConnect {
    type Error = Error;

    fn nip04_encrypt_async<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, Self::Error>> + Send + 'a>> {
        Box::pin(async move { self._nip04_encrypt(*public_key, content.to_string()).await })
    }

    fn nip04_decrypt_async<'a>(
        &'a self,
        public_key: &'a PublicKey,
        encrypted_content: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self._nip04_decrypt(*public_key, encrypted_content.to_string())
                .await
        })
    }
}

impl AsyncNip44 for NostrConnect {
    type Error = Error;

    fn nip44_encrypt_async<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, Self::Error>> + Send + 'a>> {
        Box::pin(async move { self._nip44_encrypt(*public_key, content.to_string()).await })
    }

    fn nip44_decrypt_async<'a>(
        &'a self,
        public_key: &'a PublicKey,
        payload: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, Self::Error>> + Send + 'a>> {
        Box::pin(async move { self._nip44_decrypt(*public_key, payload.to_string()).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_connect_response() {
        assert!(is_valid_connect_response(&ResponseResult::Ack, None));
        assert!(is_valid_connect_response(
            &ResponseResult::ConnectSecret("secret".to_string()),
            Some("secret")
        ));
        assert!(!is_valid_connect_response(
            &ResponseResult::ConnectSecret("secret".to_string()),
            Some("other_secret")
        ));
        assert!(!is_valid_connect_response(
            &ResponseResult::ConnectSecret("secret".to_string()),
            None
        ));
    }
}
