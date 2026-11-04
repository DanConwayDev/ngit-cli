//! Delayed, non-interactive diagnostics for remote signer operations.

use std::{collections::BTreeMap, future::Future, time::Duration};

use anyhow::Result;
use nostr::prelude::EventId;
use nostr_sdk::relay::{EventSendStatus, RelayStatus};
use tokio::sync::broadcast::error::RecvError;

use crate::{
    output_mode::TransientLine,
    remote_signer::{ConnectProgress, NostrConnect},
};

#[derive(Debug, PartialEq)]
enum Delivery {
    Accepted,
    Unconfirmed,
    Failed(String),
}

impl Delivery {
    fn label(&self) -> String {
        match self {
            Self::Accepted => "accepted (signer receipt unconfirmed)".to_string(),
            Self::Unconfirmed => "sent; acceptance unconfirmed".to_string(),
            Self::Failed(reason) => format!("request failed: {reason}"),
        }
    }
}

struct Report {
    heading: String,
    relays: Vec<String>,
    details: Vec<String>,
    action: String,
}

#[derive(Default)]
struct RequestProgress {
    method: String,
    relays: BTreeMap<String, Delivery>,
    answered: bool,
}

#[derive(Default)]
struct Progress {
    requests: BTreeMap<EventId, RequestProgress>,
    current: Option<EventId>,
    subscriptions: BTreeMap<String, String>,
}

impl Progress {
    /// Returns whether a relay failure should be shown immediately.
    fn update(&mut self, event: ConnectProgress) -> bool {
        match event {
            ConnectProgress::Paired => {}
            ConnectProgress::Ready { relay_url } => {
                self.subscriptions.insert(
                    relay_url.to_string(),
                    "response subscription ready".to_string(),
                );
            }
            ConnectProgress::Listening { output } => {
                for (url, ()) in output.success {
                    self.subscriptions
                        .insert(url.to_string(), "response subscription queued".to_string());
                }
                let failed = !output.failed.is_empty();
                for (url, error) in output.failed {
                    self.subscriptions.insert(
                        url.to_string(),
                        format!("response subscription failed: {error}"),
                    );
                }
                return failed;
            }
            ConnectProgress::Request { event_id, method } => {
                self.current = Some(event_id);
                self.requests.insert(
                    event_id,
                    RequestProgress {
                        method: method.to_string(),
                        ..Default::default()
                    },
                );
            }
            ConnectProgress::Sent { event_id, output } => {
                if let Some(request) = self.requests.get_mut(&event_id) {
                    for (url, status) in output.success {
                        request.relays.insert(
                            url.to_string(),
                            match status {
                                EventSendStatus::Ack(_) => Delivery::Accepted,
                                EventSendStatus::Sent => Delivery::Unconfirmed,
                            },
                        );
                    }
                    let failed = !output.failed.is_empty();
                    for (url, error) in output.failed {
                        request
                            .relays
                            .insert(url.to_string(), Delivery::Failed(error));
                    }
                    return failed;
                }
            }
            ConnectProgress::Response { event_id } => {
                if let Some(request) = self.requests.get_mut(&event_id) {
                    request.answered = true;
                }
            }
        }
        false
    }

    fn report_lines(
        &self,
        description: &str,
        relays: &[(String, RelayStatus)],
        verbose: bool,
    ) -> Report {
        let current = self.current.and_then(|id| self.requests.get(&id));
        let pending = current.filter(|request| !request.answered);
        let replied = current.is_some_and(|request| request.answered);
        let mut heading = format!(
            "{}: {description}",
            if replied { "Signer replied" } else { "Waiting" }
        );
        if let Some(request) = pending {
            match request.method.as_str() {
                "connect" => heading.push_str(" (signer connection approval)"),
                "get_public_key" => heading.push_str(" (public-key approval)"),
                _ => {}
            }
        }
        let relay_lines = relays
            .iter()
            .map(|(url, connection)| {
                let delivery = pending
                    .and_then(|request| request.relays.get(url))
                    .map(Delivery::label)
                    .unwrap_or_else(|| {
                        if pending.is_some() {
                            "acceptance unconfirmed".to_string()
                        } else if replied {
                            "no pending request".to_string()
                        } else {
                            "waiting for signer connection".to_string()
                        }
                    });
                format!("{url}: {connection}; {delivery}")
            })
            .collect();
        let mut details = Vec::new();
        if verbose {
            for (url, subscription) in &self.subscriptions {
                details.push(format!("{url}: {subscription}"));
            }
            for (id, request) in &self.requests {
                details.push(format!(
                    "{} request {}: {}",
                    request.method,
                    id.to_hex(),
                    if request.answered {
                        "signer replied"
                    } else {
                        "awaiting response"
                    }
                ));
            }
        }
        let all_failed = !relays.is_empty()
            && pending.is_some_and(|request| {
                relays
                    .iter()
                    .all(|(url, _)| matches!(request.relays.get(url), Some(Delivery::Failed(_))))
            });
        let accepted = pending.is_some_and(|request| {
            request
                .relays
                .values()
                .any(|status| matches!(status, Delivery::Accepted))
        });
        let disconnected = !relays.is_empty()
            && relays
                .iter()
                .all(|(_, status)| *status != RelayStatus::Connected);
        let action = if replied {
            "The signer replied. Check the command error below before trying again."
        } else if all_failed {
            "No relay accepted this request. Check ngit's connections and the relay errors above."
        } else if accepted {
            "Check your signer: approve the request and verify its connections to the accepted relays."
        } else if pending.is_some() {
            "Delivery is unconfirmed. Check the relay connections above and your signer's pending approvals."
        } else if disconnected {
            "Check your network and the signer relay addresses; no relay is connected."
        } else {
            "Open your signer and scan or paste the pairing URI; check its relay connections."
        };
        let recovery = if all_failed {
            " After fixing the relay connection, cancel and rerun the command."
        } else if pending.is_some() {
            " If it missed the request while disconnected, cancel and rerun the command."
        } else {
            ""
        };
        Report {
            heading,
            relays: relay_lines,
            details,
            action: format!("{action}{recovery}"),
        }
    }

    async fn report(&self, connect: &NostrConnect, description: &str) {
        let statuses = connect.status().await;
        let relays: Vec<_> = connect
            .relays()
            .iter()
            .map(|url| {
                let status = statuses
                    .get(url)
                    .copied()
                    .unwrap_or(RelayStatus::Initialized);
                (url.to_string(), status)
            })
            .collect();
        let report = self.report_lines(description, &relays, crate::output_mode::is_verbose());
        eprintln!(
            "{}",
            console::Style::new()
                .for_stderr()
                .bold()
                .apply_to(report.heading)
        );
        let muted = console::Style::new().for_stderr().dim();
        for line in report.relays.into_iter().chain(report.details) {
            eprintln!("  {}", muted.apply_to(line));
        }
        eprintln!("{}", report.action);
    }
}

/// Observe one operation without adding prompts or changing its retry policy.
pub async fn run_remote_operation<T>(
    connect: &NostrConnect,
    description: &str,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    run_remote_operation_when_listening(connect, description, operation, || {}).await
}

/// Run a callback once a relay confirms the response subscription with EOSE.
/// This lets URI-based callers expose their URI only after starting to listen.
pub async fn run_remote_operation_when_listening<T>(
    connect: &NostrConnect,
    description: &str,
    operation: impl Future<Output = Result<T>>,
    mut listening: impl FnMut(),
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + SIGNER_TIMEOUT;
    let _monitor =
        wait_for_signer_until(deadline, async { Ok(connect.lock_operation().await) }).await?;
    let mut events = connect.progress();
    let term = console::Term::stderr();
    let mut line = Some(TransientLine::write(&term, description)?);
    let mut clear_line = || -> Result<()> {
        if let Some(line) = line.take() {
            // Pipes must retain plain progress lines, especially the pairing
            // URI consumed by agents while the command is still running.
            if term.is_term() {
                line.clear()?;
            }
        }
        Ok(())
    };
    let mut state = Progress::default();
    let delay = tokio::time::sleep(Duration::from_secs(5));
    let operation = wait_for_signer_until(deadline, operation);
    tokio::pin!(delay, operation);
    let mut expanded = false;
    let mut listened = false;
    loop {
        tokio::select! {
            result = &mut operation => {
                clear_line()?;
                if result.is_err() {
                    while let Ok(event) = events.try_recv() { state.update(event); }
                    state.report(connect, description).await;
                }
                return result;
            }
            _ = &mut delay, if !expanded => {
                clear_line()?;
                state.report(connect, description).await;
                expanded = true;
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if matches!(&event, ConnectProgress::Ready { .. }) && !listened {
                            clear_line()?;
                            listening();
                            listened = true;
                        }
                        let public_key_request = matches!(&event, ConnectProgress::Request { method: nostr::nips::nip46::NostrConnectMethod::GetPublicKey, .. });
                        if public_key_request && !expanded && !crate::output_mode::is_quiet() {
                            clear_line()?;
                            eprintln!("{}", console::Style::new().for_stderr().bold().apply_to(
                                "Signer paired; waiting for public-key approval. Check your signer."
                            ));
                        }
                        let outcomes = matches!(event, ConnectProgress::Sent { .. });
                        let failed = state.update(event);
                        if failed || (expanded && (outcomes || public_key_request)) {
                            clear_line()?;
                            state.report(connect, description).await;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        eprintln!("Signer progress updates were missed; request acceptance may be unknown.");
                    }
                    Err(RecvError::Closed) => return operation.await,
                }
            }
        }
    }
}

/// Bound the complete operation, including any connection handshake. The SDK's
/// timeout applies separately to individual requests and cannot bound the sum.
const SIGNER_TIMEOUT: Duration = Duration::from_secs(120);

const SIGNER_TIMEOUT_GUIDANCE: &str = "remote signer did not respond in time. Open your signing app, check pending approvals and its signer-relay connections, then rerun the command. Requests are ephemeral: reconnecting the signer may not recover an earlier request.";

async fn wait_for_signer_until<T>(
    deadline: tokio::time::Instant,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(Err(error))
            if error
                .downcast_ref::<nostr_connect::error::Error>()
                .is_some_and(|error| error.kind() == nostr_connect::error::ErrorKind::Timeout) =>
        {
            Err(error.context(SIGNER_TIMEOUT_GUIDANCE))
        }
        Ok(result) => result,
        Err(error) => Err(anyhow::Error::new(error).context(SIGNER_TIMEOUT_GUIDANCE)),
    }
}

#[cfg(test)]
async fn wait_for_signer<T>(operation: impl Future<Output = Result<T>>) -> Result<T> {
    wait_for_signer_until(tokio::time::Instant::now() + SIGNER_TIMEOUT, operation).await
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use nostr::{nips::nip46::NostrConnectMethod, prelude::RelayUrl};
    use nostr_sdk::client::Output;

    use super::*;

    fn request_id(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    #[test]
    fn sent_without_ack_never_claims_acceptance() {
        let id = request_id(1);
        let relay = RelayUrl::parse("wss://signer.example.com").unwrap();
        let mut progress = Progress::default();
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::SignEvent,
        });
        let mut output = Output::new(id);
        output.success.insert(relay, EventSendStatus::Sent);
        assert!(!progress.update(ConnectProgress::Sent {
            event_id: id,
            output
        }));
        assert!(
            progress.requests[&id]
                .relays
                .values()
                .all(|value| matches!(value, Delivery::Unconfirmed))
        );
        assert!(!progress.requests[&id].answered);
    }

    #[test]
    fn relay_failure_and_signer_response_are_separate_and_request_scoped() {
        let first = request_id(1);
        let second = request_id(2);
        let mut progress = Progress::default();
        for id in [first, second] {
            progress.update(ConnectProgress::Request {
                event_id: id,
                method: NostrConnectMethod::GetPublicKey,
            });
        }
        let mut output = Output::new(first);
        output.failed.insert(
            RelayUrl::parse("wss://signer.example.com").unwrap(),
            "blocked".to_string(),
        );
        assert!(progress.update(ConnectProgress::Sent {
            event_id: first,
            output
        }));
        progress.update(ConnectProgress::Response { event_id: second });
        assert!(!progress.requests[&first].answered);
        assert!(progress.requests[&second].answered);
        assert!(progress.requests[&second].relays.is_empty());
    }

    #[test]
    fn concise_report_only_describes_current_pending_request() {
        let relay = "wss://signer.example.com/".to_string();
        let first = request_id(1);
        let current = request_id(2);
        let mut progress = Progress::default();
        progress.requests.insert(
            first,
            RequestProgress {
                method: "connect".to_string(),
                relays: BTreeMap::from([(relay.clone(), Delivery::Accepted)]),
                answered: true,
            },
        );
        progress.update(ConnectProgress::Request {
            event_id: current,
            method: NostrConnectMethod::SignEvent,
        });
        progress
            .subscriptions
            .insert(relay.clone(), "response subscription ready".to_string());
        let relays = [(relay, RelayStatus::Connected)];
        let report =
            progress.report_lines("signing event (repository announcement)", &relays, false);
        assert!(report.heading.contains("repository announcement"));
        assert_eq!(report.relays.len(), relays.len());
        assert!(report.relays[0].contains("acceptance unconfirmed"));
        assert!(!report.relays[0].contains("accepted"));
        assert!(report.details.is_empty());
        assert!(report.action.contains("Delivery is unconfirmed"));
        assert!(!report.action.contains("press r"));
        let verbose = progress.report_lines("signing", &relays, true);
        assert!(
            verbose
                .details
                .iter()
                .any(|line| line.contains(&first.to_hex()))
        );
        assert!(
            verbose
                .details
                .iter()
                .any(|line| line.contains("subscription ready"))
        );
    }

    #[test]
    fn recovery_distinguishes_accepted_unconfirmed_and_failed_delivery() {
        let id = request_id(1);
        let url = "wss://signer.example.com/".to_string();
        let relays = [(url.clone(), RelayStatus::Connected)];
        let mut progress = Progress::default();
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::SignEvent,
        });
        for (status, expected) in [
            (Delivery::Accepted, "accepted relays"),
            (Delivery::Unconfirmed, "Delivery is unconfirmed"),
            (Delivery::Failed("blocked".to_string()), "No relay accepted"),
        ] {
            progress
                .requests
                .get_mut(&id)
                .unwrap()
                .relays
                .insert(url.clone(), status);
            let report = progress.report_lines("signing", &relays, false);
            assert!(report.action.contains(expected));
            assert!(report.action.contains("rerun the command"));
            assert_eq!(report.relays.len(), 1);
            if expected == "No relay accepted" {
                assert!(!report.action.contains("missed"));
            }
        }
    }

    #[test]
    fn pairing_and_completed_response_do_not_offer_resending() {
        let mut progress = Progress::default();
        let pairing = progress.report_lines("pairing", &[], false);
        assert!(pairing.action.contains("pairing URI"));
        assert!(!pairing.action.contains("press r"));
        let id = request_id(1);
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::GetPublicKey,
        });
        progress.update(ConnectProgress::Response { event_id: id });
        let completed = progress.report_lines("pairing", &[], false);
        assert!(completed.heading.contains("Signer replied"));
        assert!(!completed.action.contains("pairing URI"));
        assert!(!completed.action.contains("press r"));
    }

    #[test]
    fn disconnected_pairing_points_to_network_and_relay_configuration() {
        let report = Progress::default().report_lines(
            "pairing",
            &[(
                "wss://signer.example.com/".to_string(),
                RelayStatus::Disconnected,
            )],
            false,
        );
        assert!(report.action.contains("network"));
        assert!(report.action.contains("relay addresses"));
        assert!(!report.action.contains("scan"));
    }

    #[tokio::test(start_paused = true)]
    async fn operation_waiting_for_another_monitor_remains_bounded() {
        let keys = nostr::prelude::Keys::generate();
        let uri = nostr::nips::nip46::NostrConnectUri::client(keys.public_key(), vec![], "test");
        let connect = NostrConnect::new(uri, keys, SIGNER_TIMEOUT, None).unwrap();
        let held = connect.lock_operation().await;
        let started = tokio::time::Instant::now();
        let error = run_remote_operation(&connect, "queued signer operation", async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), SIGNER_TIMEOUT);
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        drop(held);
        assert_eq!(
            run_remote_operation(&connect, "next operation", async { Ok(42) })
                .await
                .unwrap(),
            42
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_covers_handshake_and_request_and_drops_pending_work() {
        struct PendingWork(Arc<AtomicBool>);
        impl Drop for PendingWork {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let pending_work = PendingWork(Arc::clone(&dropped));
        let keys = nostr::prelude::Keys::generate();
        let uri = nostr::nips::nip46::NostrConnectUri::client(keys.public_key(), vec![], "test");
        let connect =
            crate::remote_signer::NostrConnect::new(uri, keys, SIGNER_TIMEOUT, None).unwrap();
        let started = tokio::time::Instant::now();
        let error = run_remote_operation(&connect, "waiting for test signer...", async move {
            let _pending_work = pending_work;
            // Passage of time represents the two protocol stages under test.
            tokio::time::sleep(SIGNER_TIMEOUT / 2).await;
            std::future::pending::<anyhow::Result<()>>().await
        })
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), SIGNER_TIMEOUT);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        assert!(error.to_string().contains("rerun the command"));
    }

    #[tokio::test(start_paused = true)]
    async fn signer_result_returns_without_waiting_for_deadline() {
        let started = tokio::time::Instant::now();
        assert_eq!(wait_for_signer(async { Ok(42) }).await.unwrap(), 42);
        assert_eq!(started.elapsed(), std::time::Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn signer_rejection_preserves_cause_without_retry_advice() {
        let error = nostr_connect::error::Error::with_static_message(
            nostr_connect::error::ErrorKind::Rejected,
            "approval denied",
        );
        let error = wait_for_signer(async { Err::<(), _>(anyhow::Error::new(error)) })
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<nostr_connect::error::Error>()
                .unwrap()
                .kind(),
            nostr_connect::error::ErrorKind::Rejected
        );
        assert!(!error.to_string().contains("rerun"));
    }

    #[tokio::test(start_paused = true)]
    async fn sdk_timeout_receives_same_recovery_guidance() {
        let error = nostr_connect::error::Error::simple(nostr_connect::error::ErrorKind::Timeout);
        let error = wait_for_signer(async { Err::<(), _>(anyhow::Error::new(error)) })
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<nostr_connect::error::Error>()
                .unwrap()
                .kind(),
            nostr_connect::error::ErrorKind::Timeout
        );
        assert!(error.to_string().contains("rerun the command"));
    }
}
