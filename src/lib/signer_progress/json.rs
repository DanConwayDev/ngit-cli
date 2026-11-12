//! Semantic signer observations without terminal reports or request payloads.
use serde_json::json;

use crate::{remote_signer::ConnectProgress, reporting::Progress};

pub(super) struct JsonProgress {
    progress: Progress,
    current: Option<nostr::prelude::EventId>,
    request: usize,
    detailed: bool,
}

impl JsonProgress {
    pub(super) fn new(description: &str) -> Self {
        Self {
            progress: Progress::new("signer", json!({"description":description})),
            current: None,
            request: 0,
            detailed: crate::output_mode::is_verbose(),
        }
    }

    pub(super) fn update(&mut self, event: &ConnectProgress) {
        match event {
            ConnectProgress::Request {
                event_id,
                method,
                retry,
                ..
            } => {
                let retrying = self.current == Some(*event_id);
                if !retrying {
                    self.current = Some(*event_id);
                    self.request += 1;
                }
                self.progress.update(if retrying { "retrying" } else { "waiting_for_response" },
                    json!({"request":self.request, "method":method.to_string(),
                        "attempt":retry.map(|r| 3 - r.sends_remaining),
                        "timeout_secs":retry.map(|r| r.deadline.saturating_duration_since(tokio::time::Instant::now()).as_secs()),
                        "action":"check_signer"}));
            }
            ConnectProgress::Sent { output, .. } if self.detailed || !output.failed.is_empty() => {
                // Acceptance is relay evidence, never signer approval.
                self.progress.update("request_delivery", json!({"request":self.request,
                    "relays_accepted": output.success.values().filter(|s| matches!(s, nostr_sdk::relay::EventSendStatus::Ack(_))).count(),
                    "relays_unconfirmed":output.success.values().filter(|s| matches!(s, nostr_sdk::relay::EventSendStatus::Sent)).count(),
                    "failures":output.failed}));
            }
            ConnectProgress::SubscriptionUnavailable {
                relay_url,
                reason: Some(reason),
            } => {
                self.progress.update(
                    "listener_unavailable",
                    json!({"relay":relay_url,"reason":reason}),
                );
            }
            ConnectProgress::Listening { output } if !output.failed.is_empty() => {
                self.progress
                    .update("listener_failed", json!({"failures":output.failed}));
            }
            ConnectProgress::Ready { relay_url } if self.detailed => {
                self.progress
                    .update("listener_ready", json!({"relay":relay_url}));
            }
            // The operation result, after validation, is authoritative. A response
            // alone does not establish successful signing, encryption or pairing.
            _ => {}
        }
    }

    pub(super) fn finish(&mut self, error: Option<&str>) {
        self.progress.finish(
            if error.is_some() {
                "failed"
            } else {
                "completed"
            },
            json!({"error":error}),
        );
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventId, NostrConnectMethod, RelayUrl};

    use super::*;
    #[test]
    fn normal_signer_stream_omits_listener_chatter_and_waits_for_validated_result() {
        let (progress, bytes) = crate::reporting::tests::progress("signer");
        let mut renderer = JsonProgress {
            progress,
            current: None,
            request: 0,
            detailed: false,
        };
        let id = EventId::from_byte_array([1; 32]);
        renderer.update(&ConnectProgress::Request {
            event_id: id,
            method: NostrConnectMethod::SignEvent,
            can_resend: true,
            retry: None,
        });
        for _ in 0..100 {
            renderer.update(&ConnectProgress::Ready {
                relay_url: RelayUrl::parse("wss://relay.example").unwrap(),
            });
        }
        renderer.update(&ConnectProgress::Response { event_id: id });
        assert_eq!(crate::reporting::tests::records(&bytes).len(), 2);
        renderer.finish(None);
        let records = crate::reporting::tests::records(&bytes);
        assert_eq!(records.len(), 3);
        assert_eq!(records[1]["state"], "waiting_for_response");
        assert_eq!(records[1]["details"]["method"], "sign_event");
        assert_eq!(records[2]["state"], "completed");
        assert!(
            !String::from_utf8(bytes.lock().unwrap().clone())
                .unwrap()
                .contains(&id.to_hex())
        );
    }
}
