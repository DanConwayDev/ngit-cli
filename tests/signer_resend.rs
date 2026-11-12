//! Explicit resends repeat delivery of one request, never a command or
//! operation.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{Stream, StreamExt};
use ngit::remote_signer::{ConnectProgress, NostrConnect};
use nostr::{nips::nip44, prelude::*};
use nostr_sdk::client::{Client, ClientNotification};
use test_harness::Harness;

const DEADLINE: Duration = Duration::from_secs(10);

async fn next_request(
    notifications: &mut (impl Stream<Item = ClientNotification> + Unpin),
    signer: &Keys,
) -> Result<(Event, NostrConnectMessage)> {
    tokio::time::timeout(DEADLINE, async {
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                if event.kind == Kind::NostrConnect {
                    let text = nip44::decrypt(signer.secret_key(), &event.pubkey, &event.content)?;
                    let message = NostrConnectMessage::from_json(text)?;
                    return Ok((*event, message));
                }
            }
        }
        bail!("signer notification stream ended before a request")
    })
    .await?
}

async fn wait_for_listening(
    progress: &mut tokio::sync::broadcast::Receiver<ConnectProgress>,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if matches!(progress.recv().await?, ConnectProgress::Ready { .. }) {
                return Ok(());
            }
        }
    })
    .await?
}

async fn respond(
    client: &Client,
    signer: &Keys,
    recipient: PublicKey,
    id: &str,
    result: ResponseResult,
) -> Result<()> {
    let event = NostrConnectEventBuilder::new(
        recipient,
        NostrConnectMessage::response(id, NostrConnectResponse::with_result(result)),
    )
    .finalize(signer)?;
    client.send_event(&event).await?;
    Ok(())
}

#[tokio::test]
async fn resend_refreshes_envelope_and_accepts_a_late_original_response() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("signer")
    .build()
    .await?;
    let relay = RelayUrl::parse(harness.relay("signer").url())?;
    let signer = Keys::generate();
    let user = Keys::generate();
    let app = Keys::generate();
    let wire = Client::default();
    wire.add_relay(relay.clone()).await?;
    wire.connect().await;
    let mut notifications = wire.notifications();
    let subscription = wire
        .subscribe(
            Filter::new()
                .pubkey(signer.public_key())
                .kind(Kind::NostrConnect)
                .limit(0),
        )
        .await?;
    tokio::time::timeout(DEADLINE, async {
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Message { message, .. } = notification {
                if matches!(message.as_ref(), RelayMessage::EndOfStoredEvents(id) if id.as_ref() == &subscription.value) {
                    return Ok(());
                }
            }
        }
        bail!("signer subscription was not ready")
    }).await??;

    let connect = Arc::new(NostrConnect::new(
        NostrConnectUri::Bunker {
            remote_signer_public_key: signer.public_key(),
            relays: vec![relay],
            secret: None,
        },
        app.clone(),
        DEADLINE,
        None,
    )?);
    connect.non_secure_set_user_public_key(user.public_key())?;
    let unsigned = EventBuilder::new(Kind::TextNote, "unchanged signing payload")
        .finalize_unsigned(user.public_key());
    let expected = user.sign_event(unsigned.clone())?;
    let mut progress = connect.progress();
    let task = {
        let connect = Arc::clone(&connect);
        tokio::spawn(async move {
            ngit::signer_progress::run_remote_operation(&connect, "Signing", async {
                Ok(connect.sign_event_async(unsigned).await?)
            })
            .await
        })
    };
    let (_, handshake) = next_request(&mut notifications, &signer).await?;
    wait_for_listening(&mut progress)
        .await
        .context("bunker response subscription readiness")?;
    assert!(matches!(
        handshake.clone().to_request()?,
        NostrConnectRequest::Connect { secret: None, .. }
    ));
    respond(
        &wire,
        &signer,
        app.public_key(),
        handshake.id(),
        ResponseResult::Ack,
    )
    .await?;
    let (original, request) = next_request(&mut notifications, &signer).await?;
    connect.resend_request(original.id);
    let (resent, repeated) = next_request(&mut notifications, &signer).await?;
    assert_ne!(resent.id, original.id);
    assert_ne!(resent.content, original.content);
    assert_eq!(repeated.id(), request.id());
    assert_eq!(
        repeated.clone().to_request()?,
        request.clone().to_request()?
    );

    // Respond using the original request after its retransmission. Either
    // delivery's first final response must finish this one signing operation.
    respond(
        &wire,
        &signer,
        app.public_key(),
        request.id(),
        ResponseResult::SignEvent(Box::new(expected.clone())),
    )
    .await?;
    assert_eq!(tokio::time::timeout(DEADLINE, task).await???, expected);

    let next_unsigned =
        EventBuilder::new(Kind::TextNote, "next operation").finalize_unsigned(user.public_key());
    let next_expected = user.sign_event(next_unsigned.clone())?;
    let mut next_progress = connect.progress();
    let next_task = {
        let connect = Arc::clone(&connect);
        tokio::spawn(async move {
            ngit::signer_progress::run_remote_operation(&connect, "Signing again", async {
                Ok(connect.sign_event_async(next_unsigned).await?)
            })
            .await
        })
    };
    wait_for_listening(&mut next_progress)
        .await
        .context("established response subscription readiness")?;
    let (next_event, next_message) = next_request(&mut notifications, &signer).await?;
    assert_ne!(next_message.id(), request.id());
    // A stale button press and late duplicate reply must not resend or satisfy
    // the next operation. A bounded absence check covers both regressions.
    connect.resend_request(original.id);
    connect.resend_request(resent.id);
    respond(
        &wire,
        &signer,
        app.public_key(),
        request.id(),
        ResponseResult::SignEvent(Box::new(expected)),
    )
    .await?;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(250),
            next_request(&mut notifications, &signer)
        )
        .await
        .is_err()
    );
    assert!(!next_task.is_finished());
    connect.resend_request(next_event.id);
    let (_, next_repeated) = next_request(&mut notifications, &signer).await?;
    assert_eq!(next_repeated.id(), next_message.id());
    respond(
        &wire,
        &signer,
        app.public_key(),
        next_repeated.id(),
        ResponseResult::SignEvent(Box::new(next_expected.clone())),
    )
    .await?;
    assert_eq!(
        tokio::time::timeout(DEADLINE, next_task).await???,
        next_expected
    );
    Ok(())
}

#[tokio::test]
async fn one_time_pairing_secret_cannot_be_resent() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("signer")
    .build()
    .await?;
    let connect = NostrConnect::new(
        NostrConnectUri::Bunker {
            remote_signer_public_key: Keys::generate().public_key(),
            relays: vec![RelayUrl::parse(harness.relay("signer").url())?],
            secret: Some("one-use-secret".to_string()),
        },
        Keys::generate(),
        DEADLINE,
        None,
    )?;
    let mut progress = connect.progress();
    let operation = connect.get_public_key_async();
    tokio::pin!(operation);
    let request = tokio::time::timeout(DEADLINE, async {
        let mut request = None;
        loop {
            tokio::select! {
                result = &mut operation => bail!("absent signer unexpectedly answered: {result:?}"),
                event = progress.recv() => match event? {
                    ConnectProgress::Request { event_id, method, can_resend, .. } => {
                        assert_eq!(method, NostrConnectMethod::Connect);
                        assert!(!can_resend);
                        request = Some(event_id);
                    },
                    ConnectProgress::Sent { event_id, .. } => {
                        assert_eq!(Some(event_id), request);
                        return Ok(event_id);
                    },
                    _ => {},
                }
            }
        }
    })
    .await??;
    connect.resend_request(request);
    let outcome: std::result::Result<Result<()>, _> = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            tokio::select! {
                result = &mut operation => bail!("absent signer unexpectedly answered: {result:?}"),
                event = progress.recv() => if let ConnectProgress::Request { .. } = event? {
                    bail!("one-use pairing secret was resent");
                }
            }
        }
    }).await;
    assert!(
        outcome.is_err(),
        "a resend must not repeat a one-use pairing handshake"
    );
    Ok(())
}

async fn next_sent_attempt(
    progress: &mut tokio::sync::broadcast::Receiver<ConnectProgress>,
) -> Result<(EventId, ngit::remote_signer::RetryStatus)> {
    let mut observed = Vec::new();
    tokio::time::timeout(DEADLINE, async {
        let mut attempt = None;
        loop {
            let event = progress.recv().await?;
            observed.push(format!("{event:?}"));
            match event {
                ConnectProgress::Request {
                    event_id,
                    retry: Some(retry),
                    ..
                } => {
                    attempt = Some((event_id, retry));
                }
                ConnectProgress::Sent { event_id, output } => {
                    assert!(
                        !output.success.is_empty(),
                        "attempt did not reach the relay: {output:?}"
                    );
                    let attempt = attempt.context("publication outcome preceded its request")?;
                    assert_eq!(attempt.0, event_id);
                    return Ok(attempt);
                }
                _ => {}
            }
        }
    })
    .await
    .with_context(|| format!("waiting for a published attempt; observed: {observed:?}"))?
}

// Advance only while no wire result is awaited, then resume the real clock so
// the loopback relay can process the next send. No wall-clock sleeps are used.
async fn advance_to(instant: tokio::time::Instant) {
    tokio::time::pause();
    tokio::time::advance(instant.saturating_duration_since(tokio::time::Instant::now())).await;
    tokio::time::resume();
}

#[tokio::test]
async fn automatic_delivery_stops_after_three_total_sends() -> Result<()> {
    assert_retry_budget(0).await
}

#[tokio::test]
async fn manual_delivery_restarts_the_budget_beyond_the_original_deadline() -> Result<()> {
    assert_retry_budget(2).await
}

async fn assert_retry_budget(manual_refreshes: usize) -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("signer")
    .build()
    .await?;
    let connect = Arc::new(NostrConnect::new(
        NostrConnectUri::Bunker {
            remote_signer_public_key: Keys::generate().public_key(),
            relays: vec![RelayUrl::parse(harness.relay("signer").url())?],
            secret: None,
        },
        Keys::generate(),
        DEADLINE,
        // Advancing this runtime's clock also advances SDK heartbeat timers.
        // Disable pings in this delivery-budget fixture so a virtual jump
        // cannot close the socket before the real loopback pong is processed.
        Some(nostr_sdk::relay::RelayOptions::new().ping(false)),
    )?);
    let mut progress = connect.progress();
    let task = {
        let connect = Arc::clone(&connect);
        tokio::spawn(async move { connect.get_public_key_async().await })
    };
    let (mut id, mut schedule) = next_sent_attempt(&mut progress)
        .await
        .context("initial request did not reach relay publication outcome")?;
    let original_deadline = schedule.deadline;
    for window in 0..=manual_refreshes {
        assert_eq!(schedule.sends_remaining, 2);
        let deadline = schedule.deadline;
        assert_eq!(deadline - schedule.next_action_at, Duration::from_secs(240));
        for remaining in [1, 0] {
            advance_to(schedule.next_action_at).await;
            let (next_id, next_schedule) = next_sent_attempt(&mut progress).await
                .with_context(|| format!("automatic send with {remaining} retries remaining in window {window}; task finished: {}", task.is_finished()))?;
            assert_ne!(next_id, id);
            assert_eq!(next_schedule.sends_remaining, remaining);
            assert!(next_schedule.deadline >= deadline);
            assert!(
                next_schedule.next_action_at - schedule.next_action_at >= Duration::from_secs(120)
            );
            assert_eq!(
                next_schedule.deadline - next_schedule.next_action_at,
                Duration::from_secs(120 * u64::from(remaining))
            );
            id = next_id;
            schedule = next_schedule;
        }
        if window < manual_refreshes {
            connect.resend_request(id);
            (id, schedule) = next_sent_attempt(&mut progress).await.with_context(|| {
                format!(
                    "manual refresh after window {window}; task finished: {}",
                    task.is_finished()
                )
            })?;
            assert!(schedule.deadline > original_deadline);
        }
    }
    assert_eq!(schedule.next_action_at, schedule.deadline);
    if manual_refreshes > 0 {
        assert!(schedule.deadline.duration_since(original_deadline) >= Duration::from_secs(480));
    }
    advance_to(schedule.deadline).await;
    let error = tokio::time::timeout(DEADLINE, task)
        .await
        .context("request did not time out after final response interval")??
        .expect_err("exhausted deliveries must time out");
    assert_eq!(error.kind(), nostr_connect::error::ErrorKind::Timeout);
    while let Ok(event) = progress.try_recv() {
        assert!(
            !matches!(event, ConnectProgress::Request { .. }),
            "unexpected fourth send"
        );
    }
    Ok(())
}
