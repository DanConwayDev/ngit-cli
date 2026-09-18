//! Publication requires delivery, while transaction callers retain raw
//! outcomes.

use anyhow::Result;
use ngit::client::{Client, Connect, Params, send_events, send_events_with_results};
use nostr::prelude::*;
use test_harness::{Harness, port::UnavailableTcpEndpoint};

#[tokio::test]
async fn publication_requires_a_relay_but_an_empty_batch_is_a_noop() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let client = Client::new(Params {
        relay_default_set: vec![],
        ..Params::default()
    });
    let event =
        EventBuilder::new(Kind::TextNote, "delivery required").finalize(&Keys::generate())?;
    let error = send_events(
        &client,
        Some(repo.dir()),
        vec![event],
        vec![],
        vec![],
        false,
        true,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no publication relays"));
    assert!(
        send_events(
            &client,
            Some(repo.dir()),
            vec![],
            vec![],
            vec![],
            false,
            true
        )
        .await?
        .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn publication_preserves_failure_details_and_accepts_partial_relay_success() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let harness = Harness::builder(
            env!("CARGO_BIN_EXE_ngit"),
            env!("CARGO_BIN_EXE_git-remote-nostr"),
        )
        .with_relay("target")
        .build()
        .await?;
        let repo = harness.fresh_repo()?;
        let unavailable = UnavailableTcpEndpoint::start().await?;
        let unavailable_url = format!("ws://{}", unavailable.addr());
        let client = Client::new(Params {
            relay_default_set: vec![],
            ..Params::default()
        });
        let keys = Keys::generate();
        let events = vec![
            EventBuilder::new(Kind::TextNote, "first").finalize(&keys)?,
            EventBuilder::new(Kind::TextNote, "second").finalize(&keys)?,
        ];

        let raw = send_events_with_results(
            &client,
            Some(repo.dir()),
            events.clone(),
            vec![unavailable_url.clone()],
            vec![],
            false,
            true,
        )
        .await?;
        assert_eq!(raw, vec![(unavailable_url.clone(), false)]);
        let error = send_events(
            &client,
            Some(repo.dir()),
            events.clone(),
            vec![unavailable_url.clone()],
            vec![],
            false,
            true,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(&unavailable_url));

        // A full copy on one relay is sufficient, including on a duplicate send.
        for _ in 0..2 {
            let outcomes = send_events(
                &client,
                Some(repo.dir()),
                events.clone(),
                vec![
                    unavailable_url.clone(),
                    harness.relay("target").url().to_owned(),
                ],
                vec![],
                false,
                true,
            )
            .await?;
            assert!(
                outcomes
                    .iter()
                    .any(|(relay, accepted)| relay == &unavailable_url && !accepted)
            );
            assert!(outcomes.iter().any(|(_, accepted)| *accepted));
            let received = harness
                .relay("target")
                .events(Filter::new().author(keys.public_key()))
                .await?;
            for event in &events {
                assert!(received.iter().any(|received| received.id == event.id));
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
