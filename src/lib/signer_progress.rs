//! Delayed diagnostics and optional terminal recovery for remote signer
//! operations.

mod countdown;
mod input;
use std::{collections::BTreeMap, future::Future, time::Duration};

use anyhow::Result;
use countdown::CountdownLine;
use input::{InputAction, ResendInput};
use nostr::prelude::EventId;
use nostr_sdk::relay::{EventSendStatus, RelayStatus};
use tokio::sync::broadcast::error::RecvError;

use crate::{
    output_mode::TransientLine,
    remote_signer::{ConnectProgress, NostrConnect, RetryStatus},
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
            Self::Accepted => "sent to relay".to_string(),
            Self::Unconfirmed => "send unconfirmed".to_string(),
            Self::Failed(reason) => format!("request failed: {reason}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Indicator {
    Accepted,
    Failed,
    Unconfirmed,
    Loading,
}

impl Indicator {
    fn render(&self, frame: u64) -> String {
        let style = console::Style::new().for_stderr();
        let spinner = indicatif::ProgressStyle::default_spinner();
        match self {
            Self::Accepted => style
                .green()
                .apply_to(spinner.get_tick_str(frame))
                .to_string(),
            Self::Failed => style.red().apply_to("✘").to_string(),
            Self::Unconfirmed | Self::Loading => style
                .yellow()
                .apply_to(spinner.get_tick_str(frame))
                .to_string(),
        }
    }
}

#[derive(Clone)]
struct RelayLine {
    indicator: Indicator,
    text: String,
}

impl RelayLine {
    fn render(&self, frame: u64) -> String {
        format!(
            " {} {}",
            self.indicator.render(frame),
            console::Style::new()
                .for_stderr()
                .color256(247)
                .apply_to(&self.text)
        )
    }
}

struct Report {
    heading: String,
    waiting: bool,
    relays: Vec<RelayLine>,
    details: Vec<String>,
    action: String,
}

impl Report {
    fn body_lines(&self) -> Vec<String> {
        let style = console::Style::new().for_stderr();
        let mut lines = vec![style.clone().bold().apply_to(&self.heading).to_string()];
        lines.extend(self.relays.iter().map(|relay| relay.render(0)));
        lines.extend(
            self.details
                .iter()
                .map(|detail| format!("  {}", style.clone().color256(247).apply_to(detail))),
        );
        lines
    }
}

#[derive(Default)]
struct RequestProgress {
    method: String,
    relays: BTreeMap<String, Delivery>,
    answered: bool,
    can_resend: bool,
    retry: Option<RetryStatus>,
}

enum SubscriptionStatus {
    Unconfirmed,
    Queued,
    Ready,
    Failed(String),
}

impl SubscriptionStatus {
    fn label(&self) -> String {
        match self {
            Self::Unconfirmed => "response subscription unconfirmed".to_string(),
            Self::Queued => "response subscription queued".to_string(),
            Self::Ready => "response subscription ready".to_string(),
            Self::Failed(reason) => format!("response subscription failed: {reason}"),
        }
    }
}

#[derive(Default)]
struct Progress {
    pairing: bool,
    finished: bool,
    requests: BTreeMap<EventId, RequestProgress>,
    current: Option<EventId>,
    subscriptions: BTreeMap<String, SubscriptionStatus>,
}

impl Progress {
    /// Returns whether a relay failure should be shown immediately.
    fn update(&mut self, event: ConnectProgress) -> bool {
        match event {
            ConnectProgress::Paired => {}
            ConnectProgress::Ready { relay_url } => {
                self.subscriptions
                    .insert(relay_url.to_string(), SubscriptionStatus::Ready);
            }
            ConnectProgress::SubscriptionUnavailable { relay_url, reason } => {
                let failed = reason.is_some();
                self.subscriptions.insert(
                    relay_url.to_string(),
                    reason.map_or(SubscriptionStatus::Unconfirmed, SubscriptionStatus::Failed),
                );
                return failed;
            }
            ConnectProgress::Listening { output } => {
                for (url, ()) in output.success {
                    self.subscriptions
                        .entry(url.to_string())
                        .or_insert(SubscriptionStatus::Queued);
                }
                let failed = !output.failed.is_empty();
                for (url, error) in output.failed {
                    self.subscriptions
                        .insert(url.to_string(), SubscriptionStatus::Failed(error));
                }
                return failed;
            }
            ConnectProgress::Request {
                event_id,
                method,
                can_resend,
                retry,
            } => {
                self.current = Some(event_id);
                self.requests.insert(
                    event_id,
                    RequestProgress {
                        method: method.to_string(),
                        can_resend,
                        retry,
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

    fn pending_resend(&self) -> Option<EventId> {
        self.current.filter(|id| {
            self.requests
                .get(id)
                .is_some_and(|request| request.can_resend && !request.answered)
        })
    }

    fn pending_retry(&self) -> Option<RetryStatus> {
        self.current
            .and_then(|id| self.requests.get(&id))
            .filter(|request| !request.answered)
            .and_then(|request| request.retry)
    }

    fn report_lines(
        &self,
        description: &str,
        relays: &[(String, RelayStatus)],
        verbose: bool,
        resend_available: bool,
    ) -> Report {
        let current = self.current.and_then(|id| self.requests.get(&id));
        let pending = current.filter(|request| !request.answered);
        let replied = current.is_some_and(|request| request.answered);
        let heading = format!(
            "{}: {description}",
            if replied {
                "Signer replied"
            } else if self.finished {
                "Signer request failed"
            } else {
                "Waiting"
            }
        );
        let relay_column_width = relays
            .iter()
            .map(|(url, _)| url.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        let relay_lines = relays
            .iter()
            .map(|(url, connection)| {
                let delivery = pending.and_then(|request| request.relays.get(url));
                let subscription_failure = match self.subscriptions.get(url) {
                    Some(SubscriptionStatus::Failed(reason)) => Some(reason),
                    _ => None,
                };
                let disconnected = matches!(
                    connection,
                    RelayStatus::Disconnected
                        | RelayStatus::Terminated
                        | RelayStatus::Banned
                        | RelayStatus::Shutdown
                );
                let indicator = if disconnected
                    || subscription_failure.is_some()
                    || matches!(delivery, Some(Delivery::Failed(_)))
                {
                    Indicator::Failed
                } else {
                    match delivery {
                        Some(Delivery::Accepted)
                            if *connection == RelayStatus::Connected
                                && matches!(
                                    self.subscriptions.get(url),
                                    Some(SubscriptionStatus::Ready)
                                ) =>
                        {
                            Indicator::Accepted
                        }
                        Some(Delivery::Accepted) => Indicator::Loading,
                        Some(Delivery::Unconfirmed) => Indicator::Unconfirmed,
                        Some(Delivery::Failed(_)) => Indicator::Failed,
                        None => Indicator::Loading,
                    }
                };
                let subscription_ready =
                    matches!(self.subscriptions.get(url), Some(SubscriptionStatus::Ready));
                let label = match (delivery, subscription_failure) {
                    (Some(Delivery::Failed(reason)), _) => format!("request failed: {reason}"),
                    (_, Some(reason)) => format!("response subscription failed: {reason}"),
                    (Some(Delivery::Accepted), _)
                        if subscription_ready && *connection == RelayStatus::Connected =>
                    {
                        "awaiting response".to_string()
                    }
                    (Some(Delivery::Accepted), _) if subscription_ready => {
                        "response connection unavailable".to_string()
                    }
                    (Some(Delivery::Accepted), _) => "subscription pending".to_string(),
                    (Some(delivery), _) => delivery.label(),
                    _ if replied => "no pending request".to_string(),
                    _ if pending.is_some() && subscription_ready => "sending".to_string(),
                    _ if subscription_ready => "awaiting signer connection".to_string(),
                    _ => "subscription pending".to_string(),
                };
                let text = if *connection == RelayStatus::Connected {
                    format!("{url:<relay_column_width$}{label}")
                } else {
                    format!("{url:<relay_column_width$}{connection}; {label}")
                };
                RelayLine { indicator, text }
            })
            .collect();
        let mut details = Vec::new();
        if verbose {
            for (url, subscription) in &self.subscriptions {
                details.push(format!("{url}: {}", subscription.label()));
            }
            for (id, request) in &self.requests {
                details.push(format!(
                    "{} request {}: {}",
                    request.method,
                    id.to_hex(),
                    if request.answered {
                        "signer replied"
                    } else if self.current == Some(*id) {
                        "awaiting response"
                    } else {
                        "earlier attempt"
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
        let disconnected = !relays.is_empty()
            && relays
                .iter()
                .all(|(_, status)| *status != RelayStatus::Connected);
        let subscription_failed = self
            .subscriptions
            .values()
            .any(|status| matches!(status, SubscriptionStatus::Failed(_)));
        let action = if replied || self.finished {
            "Check the command error below before trying again.".to_string()
        } else if pending.is_some() && resend_available {
            let controls = "Press R to resend now.";
            if all_failed || subscription_failed {
                format!("{controls} Fix the relay errors above first.")
            } else {
                controls.to_string()
            }
        } else if all_failed {
            "No relay accepted the request. Fix the relay errors above.".to_string()
        } else if subscription_failed {
            "Fix the response subscription errors above.".to_string()
        } else if self.pairing && pending.is_some_and(|request| request.method == "get_public_key")
        {
            PUBLIC_KEY_APPROVAL_GUIDANCE.to_string()
        } else if pending.is_some() {
            "Check signer approvals and relay connections.".to_string()
        } else if disconnected {
            "Check your network and signer relay addresses; no relay is connected.".to_string()
        } else {
            "Open your signer and scan or paste the pairing URI.".to_string()
        };
        Report {
            heading,
            waiting: !self.finished && !replied,
            relays: relay_lines,
            details,
            action,
        }
    }

    async fn report(
        &self,
        connect: &NostrConnect,
        description: &str,
        resend_available: bool,
    ) -> Report {
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
        self.report_lines(
            description,
            &relays,
            crate::output_mode::progress_policy().detailed,
            resend_available,
        )
    }
}

fn listener_callback_ready(event: &ConnectProgress, has_callback: bool, notified: bool) -> bool {
    has_callback && !notified && matches!(event, ConnectProgress::Ready { .. })
}

/// Observe one operation, optionally accepting an explicit terminal resend.
pub async fn run_remote_operation<T>(
    connect: &NostrConnect,
    description: &str,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    run_remote_operation_inner(connect, description, operation, None::<fn()>, true).await
}

/// Run a callback once a relay confirms the response subscription with EOSE.
/// This lets URI-based callers expose their URI only after starting to listen.
pub async fn run_remote_operation_when_listening<T>(
    connect: &NostrConnect,
    description: &str,
    operation: impl Future<Output = Result<T>>,
    listening: impl FnMut(),
) -> Result<T> {
    // Pairing may already have a menu reading the terminal. It owns input.
    run_remote_operation_inner(connect, description, operation, Some(listening), false).await
}

async fn run_remote_operation_inner<T>(
    connect: &NostrConnect,
    description: &str,
    operation: impl Future<Output = Result<T>>,
    mut listening: Option<impl FnMut()>,
    allow_resend: bool,
) -> Result<T> {
    let mut deadline_at = tokio::time::Instant::now() + SIGNER_TIMEOUT;
    let _monitor =
        wait_for_signer_until(deadline_at, async { Ok(connect.lock_operation().await) }).await?;
    let mut input: Option<ResendInput> = None;
    let mut input_poll = tokio::time::interval(Duration::from_millis(150));
    input_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut animation = tokio::time::interval(Duration::from_millis(400));
    animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut events = connect.progress();
    let term = console::Term::stderr();
    let policy = crate::output_mode::progress_policy();
    let mut countdown = CountdownLine::new(&term, policy);
    let mut line = Some(TransientLine::write(&term, description)?);
    let mut clear_line = || -> Result<()> {
        if let Some(line) = line.take() {
            if term.is_term() {
                line.clear()?;
            }
        }
        Ok(())
    };
    let mut state = Progress {
        pairing: listening.is_some(),
        ..Default::default()
    };
    let delay = tokio::time::sleep(Duration::from_secs(5));
    let deadline = tokio::time::sleep_until(deadline_at);
    let observer = connect.observe_subscription_progress();
    tokio::pin!(delay, deadline, operation, observer);
    let mut observing = true;
    let mut expanded = policy.detailed && !policy.quiet;
    let mut listened = false;
    let mut events_open = true;
    loop {
        tokio::select! {
            // Drain cached subscription state before starting another operation.
            // A completed signer response still wins over a coincident timeout.
            biased;
            _ = &mut observer, if observing => observing = false,
            result = &mut operation => {
                drop(input.take());
                if result.is_err() {
                    while let Ok(event) = events.try_recv() { state.update(event); }
                    state.finished = true;
                    let report = state.report(connect, description, false).await;
                    clear_line()?;
                    countdown.show_final(report)?;
                } else if !policy.quiet && (policy.detailed || (expanded && !policy.animate)) {
                    while let Ok(event) = events.try_recv() { state.update(event); }
                    state.finished = true;
                    let mut report = state.report(connect, description, false).await;
                    report.heading = format!("Signer completed: {description}");
                    report.action.clear();
                    clear_line()?;
                    countdown.show_final(report)?;
                } else {
                    clear_line()?;
                    countdown.clear()?;
                }
                return signer_result(result);
            }
            event = events.recv(), if events_open => {
                match event {
                    Ok(event) => {
                        let listener_ready = listener_callback_ready(&event, listening.is_some(), listened);
                        if listener_ready {
                            clear_line()?;
                            countdown.clear()?;
                            if let Some(callback) = listening.as_mut() { callback(); }
                            listened = true;
                        }
                        match &event {
                            ConnectProgress::Request { retry, .. } => {
                                // Transport owns the resend window. Stage changes and accepted
                                // manual sends report the exact new deadline here.
                                deadline_at = retry.map(|retry| retry.deadline)
                                    .unwrap_or_else(|| tokio::time::Instant::now() + SIGNER_TIMEOUT);
                                deadline.as_mut().reset(deadline_at);
                            }
                            ConnectProgress::Response { .. } => {
                                deadline_at = tokio::time::Instant::now() + SIGNER_TIMEOUT;
                                deadline.as_mut().reset(deadline_at);
                            }
                            _ => {},
                        }
                        let request_started = matches!(&event, ConnectProgress::Request { .. });
                        let public_key_request = matches!(&event, ConnectProgress::Request { method: nostr::nips::nip46::NostrConnectMethod::GetPublicKey, .. });
                        if public_key_request && listening.is_some() && !expanded && !policy.quiet {
                            clear_line()?;
                            countdown.clear()?;
                            eprintln!("{}", console::Style::new().for_stderr().bold().apply_to(
                                PUBLIC_KEY_APPROVAL_GUIDANCE
                            ));
                        }
                        let outcomes = matches!(event, ConnectProgress::Sent { .. } | ConnectProgress::Ready { .. } | ConnectProgress::SubscriptionUnavailable { .. });
                        let failed = state.update(event);
                        if expanded && allow_resend && policy.animate {
                            let pending = state.pending_resend();
                            if input.as_ref().map(ResendInput::event_id) != pending {
                                drop(input.take());
                                input = pending.and_then(ResendInput::open);
                            }
                        }
                        if !policy.quiet && (failed || (expanded && (outcomes || request_started || listener_ready))) {
                            let report = state.report(connect, description, input.is_some() && state.pending_resend().is_some()).await;
                            clear_line()?;
                            countdown.show(report, state.pending_retry(), deadline_at)?;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        let report = state.report(connect, description, false).await;
                        countdown.clear()?;
                        if !policy.quiet { eprintln!("Signer progress updates were missed; request acceptance may be unknown."); }
                        countdown.show(report, state.pending_retry(), deadline_at)?;
                    }
                    Err(RecvError::Closed) => events_open = false,
                }
            }
            _ = &mut deadline => {
                drop(input.take());
                state.finished = true;
                let report = state.report(connect, description, false).await;
                clear_line()?;
                countdown.show_final(report)?;
                return wait_for_signer_until(deadline_at, std::future::pending()).await;
            }
            _ = &mut delay, if !expanded && !policy.quiet => {
                expanded = true;
                if allow_resend && policy.animate {
                    input = state.pending_resend().and_then(ResendInput::open);
                }
                let report = state.report(connect, description, input.is_some()).await;
                clear_line()?;
                countdown.show(report, state.pending_retry(), deadline_at)?;
            }
            _ = animation.tick(), if expanded && policy.animate => {
                countdown.refresh(state.pending_retry(), deadline_at)?;
            }
            _ = input_poll.tick(), if input.is_some() => {
                match input.as_mut().expect("input branch requires a terminal").poll() {
                    Ok(Some(InputAction::Resend(event_id))) => connect.resend_request(event_id),
                    Ok(Some(InputAction::Cancel)) => {
                        // Match ordinary Ctrl+C termination, restoring our terminal
                        // before exit rather than waiting for SDK/background tasks.
                        crate::output_mode::set_terminal_recovery_enabled(false);
                        drop(input.take());
                        std::process::exit(130);
                    }
                    Ok(None) => {},
                    Err(_) => {
                        input = None;
                        let report = state.report(connect, description, false).await;
                        countdown.show(report, state.pending_retry(), deadline_at)?;
                    }
                }
            }
        }
    }
}

/// Bound queueing and phases without an eligible outgoing request. Eligible
/// requests report their own bounded resend window from the transport.
const SIGNER_TIMEOUT: Duration = Duration::from_secs(120);

const PUBLIC_KEY_APPROVAL_GUIDANCE: &str =
    "Approve sharing your public key in your signer; check for another notification.";

const SIGNER_TIMEOUT_GUIDANCE: &str = "remote signer did not respond in time. Open your signing app, check pending approvals and its signer-relay connections, then rerun the command. Requests are ephemeral: reconnecting the signer may not recover an earlier request.";

async fn wait_for_signer_until<T>(
    deadline: tokio::time::Instant,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(result) => signer_result(result),
        Err(error) => Err(anyhow::Error::new(error).context(SIGNER_TIMEOUT_GUIDANCE)),
    }
}

fn signer_result<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error
            .downcast_ref::<nostr_connect::error::Error>()
            .is_some_and(|error| error.kind() == nostr_connect::error::ErrorKind::Timeout)
        {
            error.context(SIGNER_TIMEOUT_GUIDANCE)
        } else {
            error
        }
    })
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
            can_resend: true,
            retry: None,
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
                can_resend: true,
                retry: None,
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
                can_resend: true,
                retry: None,
            },
        );
        progress.update(ConnectProgress::Request {
            event_id: current,
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        progress
            .subscriptions
            .insert(relay.clone(), SubscriptionStatus::Ready);
        let relays = [(relay, RelayStatus::Connected)];
        let report = progress.report_lines(
            "signing event (repository announcement)",
            &relays,
            false,
            false,
        );
        assert!(report.heading.contains("repository announcement"));
        assert_eq!(report.relays.len(), relays.len());
        assert!(report.relays[0].text.contains("sending"));
        assert!(!report.relays[0].text.contains("accepted"));
        assert!(report.details.is_empty());
        assert!(report.action.contains("Check signer approvals"));
        assert!(!report.action.contains("press r"));
        let verbose = progress.report_lines("signing", &relays, true, false);
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
    fn relay_indicators_distinguish_acceptance_uncertainty_and_failure() {
        let id = request_id(1);
        let url = "wss://signer.example.com/".to_string();
        let relays = [(url.clone(), RelayStatus::Connected)];
        let mut progress = Progress::default();
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        assert_eq!(
            progress
                .report_lines("signing", &relays, false, false)
                .relays[0]
                .indicator,
            Indicator::Loading
        );
        progress
            .subscriptions
            .insert(url.clone(), SubscriptionStatus::Ready);
        for (status, indicator, label) in [
            (Delivery::Accepted, Indicator::Accepted, "awaiting response"),
            (Delivery::Unconfirmed, Indicator::Unconfirmed, "unconfirmed"),
            (
                Delivery::Failed("blocked".to_string()),
                Indicator::Failed,
                "blocked",
            ),
        ] {
            progress
                .requests
                .get_mut(&id)
                .unwrap()
                .relays
                .insert(url.clone(), status);
            let report = progress.report_lines("signing", &relays, false, false);
            assert_eq!(report.relays.len(), 1);
            assert_eq!(report.relays[0].indicator, indicator);
            assert!(report.relays[0].text.contains(label));
            assert!(!report.action.contains("Press R"));
            let interactive = progress.report_lines("signing", &relays, false, true);
            assert!(interactive.action.starts_with("Press R to resend now."));
            if label == "blocked" {
                assert!(report.action.contains("No relay accepted"));
                assert!(interactive.action.contains("Fix the relay errors"));
            } else {
                assert_eq!(interactive.action, "Press R to resend now.");
            }
        }
        let disconnected =
            progress.report_lines("signing", &[(url, RelayStatus::Disconnected)], false, false);
        assert_eq!(disconnected.relays[0].indicator, Indicator::Failed);
        assert!(disconnected.relays[0].text.contains("Disconnected"));
    }

    #[test]
    fn subscription_failure_is_visible_even_on_a_connected_accepting_relay() {
        let url = "wss://signer.example.com/".to_string();
        let relays = [(url.clone(), RelayStatus::Connected)];
        let mut progress = Progress::default();
        progress.subscriptions.insert(
            url.clone(),
            SubscriptionStatus::Failed("auth-required".to_string()),
        );
        let pairing = progress.report_lines("pairing", &relays, false, false);
        assert_eq!(pairing.relays[0].indicator, Indicator::Failed);
        assert!(pairing.relays[0].text.contains("auth-required"));
        assert!(pairing.action.contains("subscription errors"));
        let id = request_id(1);
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        progress
            .requests
            .get_mut(&id)
            .unwrap()
            .relays
            .insert(url.clone(), Delivery::Accepted);
        let accepted = progress.report_lines("signing", &relays, false, true);
        assert_eq!(accepted.relays[0].indicator, Indicator::Failed);
        assert!(
            accepted.relays[0]
                .text
                .contains("subscription failed: auth-required")
        );
        assert!(accepted.action.starts_with("Press R"));
        assert!(accepted.action.contains("Fix the relay errors"));
        progress
            .requests
            .get_mut(&id)
            .unwrap()
            .relays
            .insert(url, Delivery::Failed("blocked request".to_string()));
        assert!(
            progress
                .report_lines("signing", &relays, false, false)
                .relays[0]
                .text
                .contains("blocked request")
        );
    }

    #[test]
    fn queued_dispatch_does_not_overwrite_an_eose_already_observed() {
        let relay = RelayUrl::parse("wss://signer.example.com").unwrap();
        let mut progress = Progress::default();
        progress.update(ConnectProgress::Ready {
            relay_url: relay.clone(),
        });
        let mut output = Output::new(nostr::prelude::SubscriptionId::generate());
        output.success.insert(relay.clone(), ());
        progress.update(ConnectProgress::Listening { output });
        assert!(matches!(
            progress.subscriptions.get(&relay.to_string()),
            Some(SubscriptionStatus::Ready)
        ));
    }

    #[test]
    fn green_awaiting_response_requires_acceptance_and_live_ready_subscription() {
        let url = "wss://signer.example.com/".to_string();
        let id = request_id(1);
        let mut progress = Progress::default();
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        progress
            .requests
            .get_mut(&id)
            .unwrap()
            .relays
            .insert(url.clone(), Delivery::Accepted);
        let relays = [(url.clone(), RelayStatus::Connected)];
        for status in [SubscriptionStatus::Unconfirmed, SubscriptionStatus::Queued] {
            progress.subscriptions.insert(url.clone(), status);
            let pending = progress.report_lines("signing", &relays, false, true);
            assert_eq!(pending.relays[0].indicator, Indicator::Loading);
            assert!(pending.relays[0].text.contains("subscription pending"));
        }
        progress.update(ConnectProgress::Ready {
            relay_url: RelayUrl::parse(&url).unwrap(),
        });
        let ready = progress.report_lines("signing", &relays, false, true);
        assert_eq!(ready.relays[0].indicator, Indicator::Accepted);
        assert!(ready.relays[0].text.contains("awaiting response"));
        assert_eq!(ready.action, "Press R to resend now.");
        progress.update(ConnectProgress::SubscriptionUnavailable {
            relay_url: RelayUrl::parse(&url).unwrap(),
            reason: None,
        });
        assert_eq!(
            progress
                .report_lines("signing", &relays, false, true)
                .relays[0]
                .indicator,
            Indicator::Loading
        );
        progress.update(ConnectProgress::Ready {
            relay_url: RelayUrl::parse(&url).unwrap(),
        });
        let offline =
            progress.report_lines("signing", &[(url, RelayStatus::Disconnected)], false, true);
        assert_eq!(offline.relays[0].indicator, Indicator::Failed);
        assert!(!offline.relays[0].text.contains("awaiting response"));
    }

    #[test]
    fn pairing_public_key_approval_remains_actionable_in_expanded_reports() {
        for verbose in [false, true] {
            let id = request_id(1);
            let mut progress = Progress {
                pairing: true,
                ..Default::default()
            };
            progress.update(ConnectProgress::Request {
                event_id: id,
                method: NostrConnectMethod::GetPublicKey,
                can_resend: true,
                retry: None,
            });
            let report = progress.report_lines("pairing signer", &[], verbose, false);
            assert_eq!(report.action, PUBLIC_KEY_APPROVAL_GUIDANCE);
            assert!(report.heading.contains("pairing signer"));

            // Ordinary signing keeps its operation description and generic guidance.
            progress.pairing = false;
            let report = progress.report_lines("signing announcement", &[], verbose, false);
            assert!(!report.action.contains("another notification"));
            assert!(report.heading.contains("signing announcement"));

            progress.pairing = true;
            progress.requests.get_mut(&id).unwrap().method = "sign_event".to_string();
            assert!(
                !progress
                    .report_lines("pairing signer", &[], verbose, false)
                    .action
                    .contains("another notification")
            );
        }
    }

    #[test]
    fn pairing_public_key_guidance_does_not_hide_relay_failures() {
        let url = "wss://signer.example/".to_string();
        let id = request_id(1);
        let mut progress = Progress {
            pairing: true,
            ..Default::default()
        };
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::GetPublicKey,
            can_resend: true,
            retry: None,
        });
        progress
            .requests
            .get_mut(&id)
            .unwrap()
            .relays
            .insert(url.clone(), Delivery::Failed("rejected".to_string()));
        let relays = [(url.clone(), RelayStatus::Connected)];
        assert!(
            progress
                .report_lines("pairing signer", &relays, false, false)
                .action
                .contains("No relay accepted")
        );
        progress.requests.get_mut(&id).unwrap().relays.clear();
        progress
            .subscriptions
            .insert(url, SubscriptionStatus::Failed("auth required".to_string()));
        assert!(
            progress
                .report_lines("pairing signer", &relays, false, false)
                .action
                .contains("subscription errors")
        );
    }

    #[test]
    fn signing_description_survives_handshake_and_public_key_requests() {
        let mut progress = Progress::default();
        let id = request_id(1);
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::Connect,
            can_resend: true,
            retry: None,
        });
        let report = progress.report_lines(
            "signing announcement",
            &[(
                "wss://signer.example.com/".to_string(),
                RelayStatus::Connected,
            )],
            false,
            false,
        );
        assert_eq!(report.heading, "Waiting: signing announcement");
        assert_eq!(
            console::strip_ansi_codes(&report.body_lines()[0]),
            report.heading
        );
        assert_ne!(report.relays[0].render(0), report.relays[0].render(5));
        progress.update(ConnectProgress::Request {
            event_id: request_id(2),
            method: NostrConnectMethod::GetPublicKey,
            can_resend: true,
            retry: None,
        });
        assert_eq!(
            progress
                .report_lines("signing announcement", &[], false, false)
                .heading,
            "Waiting: signing announcement"
        );
        progress.update(ConnectProgress::Request {
            event_id: request_id(3),
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        assert_eq!(
            progress
                .report_lines("signing announcement", &[], false, false)
                .heading,
            "Waiting: signing announcement"
        );
    }

    #[test]
    fn ordinary_ready_events_preserve_the_current_display() {
        let ready = ConnectProgress::Ready {
            relay_url: RelayUrl::parse("wss://signer.example.com").unwrap(),
        };
        assert!(!listener_callback_ready(&ready, false, false));
        assert!(listener_callback_ready(&ready, true, false));
        assert!(!listener_callback_ready(&ready, true, true));
        assert!(!listener_callback_ready(
            &ConnectProgress::Paired,
            true,
            false
        ));
    }

    #[test]
    fn relay_status_columns_align_using_full_urls() {
        let relays = [
            ("wss://a/".to_string(), RelayStatus::Connected),
            ("wss://longer.example/".to_string(), RelayStatus::Connected),
        ];
        let report = Progress::default().report_lines("pairing", &relays, false, false);
        assert_eq!(
            report.relays[0].text.find("subscription pending"),
            report.relays[1].text.find("subscription pending")
        );
        assert!(report.relays[0].text.starts_with("wss://a/"));
    }

    #[test]
    fn pairing_and_completed_response_do_not_offer_resending() {
        let mut progress = Progress::default();
        let pairing = progress.report_lines("pairing", &[], false, false);
        assert!(pairing.action.contains("pairing URI"));
        assert!(!pairing.action.contains("press r"));
        let id = request_id(1);
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::GetPublicKey,
            can_resend: true,
            retry: None,
        });
        progress.update(ConnectProgress::Response { event_id: id });
        let completed = progress.report_lines("pairing", &[], false, false);
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
            false,
        );
        assert!(report.action.contains("network"));
        assert!(report.action.contains("relay addresses"));
        assert!(!report.action.contains("scan"));
    }

    #[test]
    fn resend_is_only_offered_for_an_eligible_pending_request() {
        let mut progress = Progress::default();
        assert!(progress.pending_resend().is_none());
        let id = request_id(1);
        progress.update(ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::Connect,
            can_resend: false,
            retry: None,
        });
        assert!(progress.pending_resend().is_none());
        let latest = request_id(2);
        progress.update(ConnectProgress::Request {
            event_id: latest,
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        assert_eq!(progress.pending_resend(), Some(latest));
        let report = progress.report_lines("signing", &[], true, true);
        assert!(report.action.contains("Press R"));
        assert!(
            report
                .details
                .iter()
                .any(|line| line.ends_with("earlier attempt"))
        );
        progress.update(ConnectProgress::Response { event_id: latest });
        assert!(progress.pending_resend().is_none());
        assert!(
            !progress
                .report_lines("signing", &[], false, true)
                .action
                .contains("Press R")
        );
    }

    #[test]
    fn finished_report_does_not_offer_more_waiting_or_keyboard_input() {
        let mut progress = Progress::default();
        progress.update(ConnectProgress::Request {
            event_id: request_id(1),
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        progress.finished = true;
        let report = progress.report_lines("signing", &[], false, true);
        assert!(report.heading.contains("failed"));
        assert!(report.action.contains("command error"));
        assert!(!report.action.contains("Press R"));
        assert!(!report.action.contains("waits"));
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
