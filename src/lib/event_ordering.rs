//! Shared NIP-01 ordering policy for events that replace an earlier event.

use anyhow::Result;
use nostr::{
    Event, EventBuilder, EventId, PublicKey, Tag, Timestamp,
    event::{FinalizeUnsignedEvent, UnsignedEvent},
};

const MAX_EXPECTED_ATTEMPTS: u64 = 10_000;
const MAX_GRIND_ATTEMPTS: u128 = 100_000;
const NONCE_MARKER: &str = "ngit-created-at-tiebreak";

/// Select the NIP-01 winner: newest timestamp, then lowest event ID.
pub fn latest_event<'a>(events: impl IntoIterator<Item = &'a Event>) -> Option<&'a Event> {
    events.into_iter().max_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            // `max_by` retains the greater value. NIP-01 gives a timestamp tie
            // to the lexicographically lower event ID.
            .then_with(|| b.id.cmp(&a.id))
    })
}

/// Finalize an affected event so it sorts after `reference` under NIP-01.
///
/// The event is deliberately kept unsigned; callers must sign the returned
/// candidate exactly once.
pub fn finalize_ordered_unsigned(
    builder: EventBuilder,
    public_key: PublicKey,
    reference: Option<&Event>,
) -> Result<UnsignedEvent> {
    finalize_ordered_unsigned_at(
        builder,
        public_key,
        reference,
        Timestamp::now(),
        MAX_GRIND_ATTEMPTS,
    )
}

/// Finalize a replaceable event strictly after `reference` by timestamp.
///
/// This remains valid NIP-01 ordering, while accommodating GRASP's purgatory
/// authorization, which selects repository state events by `created_at`.
/// Repository state publication must therefore not depend on NIP-01's
/// same-second event-ID tiebreak.
pub fn finalize_strictly_later_unsigned(
    mut builder: EventBuilder,
    public_key: PublicKey,
    reference: Option<&Event>,
) -> Result<UnsignedEvent> {
    builder.tags = nostr::Tags::from_list(
        builder
            .tags
            .into_iter()
            .filter(|tag| !is_ngit_nonce(tag))
            .collect(),
    );

    let Some(created_at) = strictly_later_timestamp(reference, Timestamp::now())? else {
        return Ok(builder.finalize_unsigned(public_key));
    };
    Ok(builder
        .custom_created_at(created_at)
        .finalize_unsigned(public_key))
}

/// Return an explicit timestamp only when `now` must be advanced to sort
/// strictly after `reference` by timestamp.
pub fn strictly_later_timestamp(
    reference: Option<&Event>,
    now: Timestamp,
) -> Result<Option<Timestamp>> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    if now > reference.created_at {
        return Ok(None);
    }

    reference
        .created_at
        .as_secs()
        .checked_add(1)
        .map(Timestamp::from_secs)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("event timestamp overflow while ordering update"))
}

/// The implementation accepts the current time and attempt limit separately so
/// the timestamp policy can be exercised deterministically in unit tests.
fn finalize_ordered_unsigned_at(
    mut builder: EventBuilder,
    public_key: PublicKey,
    reference: Option<&Event>,
    now: Timestamp,
    max_grind_attempts: u128,
) -> Result<UnsignedEvent> {
    // This is an internal, one-use tiebreaker. Do not round-trip it through
    // announcement extra tags after the next normally-timestamped update.
    builder.tags = nostr::Tags::from_list(
        builder
            .tags
            .into_iter()
            .filter(|tag| !is_ngit_nonce(tag))
            .collect(),
    );

    let Some(reference) = reference else {
        return Ok(builder.finalize_unsigned(public_key));
    };

    if now > reference.created_at {
        return Ok(builder.finalize_unsigned(public_key));
    }

    if expected_attempts_at_most(&reference.id, MAX_EXPECTED_ATTEMPTS) {
        for nonce in 0..max_grind_attempts {
            let candidate = builder
                .clone()
                .custom_created_at(reference.created_at)
                .tag(ngit_nonce(nonce))
                .finalize_unsigned(public_key);
            if candidate.compute_id() < reference.id {
                return Ok(candidate);
            }
        }
    }

    let created_at = reference
        .created_at
        .as_secs()
        .checked_add(1)
        .map(Timestamp::from_secs)
        .ok_or_else(|| anyhow::anyhow!("event timestamp overflow while ordering update"))?;
    Ok(builder
        .custom_created_at(created_at)
        .finalize_unsigned(public_key))
}

fn is_ngit_nonce(tag: &Tag) -> bool {
    matches!(tag.as_slice(), [name, _, difficulty, marker] if name == "nonce" && difficulty == "0" && marker == NONCE_MARKER)
}

fn ngit_nonce(counter: u128) -> Tag {
    Tag::parse(["nonce", &counter.to_string(), "0", NONCE_MARKER]).expect("ngit nonce tag is valid")
}

fn expected_attempts_at_most(id: &EventId, maximum: u64) -> bool {
    let leading = u64::from_be_bytes(
        id.as_bytes()[..8]
            .try_into()
            .expect("event id has 32 bytes"),
    );
    // p is at least leading / 2^64. Avoid floating point and big integers:
    // expected <= maximum iff leading >= ceil(2^64 / maximum).
    leading > u64::MAX / maximum
}

#[cfg(test)]
mod tests {
    use nostr::{Keys, Kind, event::SignEvent};

    use super::*;

    #[test]
    fn latest_event_prefers_lower_id_on_timestamp_tie() {
        let keys = Keys::generate();
        let a = keys
            .sign_event(
                EventBuilder::new(Kind::TextNote, "a")
                    .custom_created_at(Timestamp::from_secs(1))
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();
        let b = keys
            .sign_event(
                EventBuilder::new(Kind::TextNote, "b")
                    .custom_created_at(Timestamp::from_secs(1))
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();
        let expected = if a.id < b.id { &a } else { &b };
        assert_eq!(latest_event([&a, &b]).unwrap().id, expected.id);
    }

    #[test]
    fn removes_only_ngit_nonce_tags() {
        assert!(is_ngit_nonce(&ngit_nonce(0)));
        assert!(!is_ngit_nonce(
            &Tag::parse(["nonce", "1", "0", "other"]).unwrap()
        ));
    }

    fn reference_with_id(id: &str, created_at: u64) -> Event {
        let keys = Keys::generate();
        let mut event = keys
            .sign_event(
                EventBuilder::new(Kind::TextNote, "reference")
                    .custom_created_at(Timestamp::from_secs(created_at))
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();
        event.id = EventId::from_hex(id).unwrap();
        event
    }

    fn candidate_builder() -> EventBuilder {
        EventBuilder::new(Kind::TextNote, "candidate")
    }

    #[test]
    fn no_reference_preserves_the_builder_timestamp() {
        let keys = Keys::generate();
        let event = finalize_ordered_unsigned_at(
            candidate_builder().custom_created_at(Timestamp::from_secs(7)),
            keys.public_key(),
            None,
            Timestamp::from_secs(10),
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(7));
        assert!(!event.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn older_reference_preserves_the_builder_timestamp() {
        let keys = Keys::generate();
        let reference = reference_with_id(&"00".repeat(32), 9);
        let event = finalize_ordered_unsigned_at(
            candidate_builder().custom_created_at(Timestamp::from_secs(10)),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(10),
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(10));
        assert!(!event.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn equal_and_future_feasible_references_are_beaten_at_the_same_timestamp() {
        let keys = Keys::generate();
        for (reference_timestamp, now) in [(10, 10), (11, 10)] {
            let reference = reference_with_id(&"ff".repeat(32), reference_timestamp);
            let event = finalize_ordered_unsigned_at(
                candidate_builder(),
                keys.public_key(),
                Some(&reference),
                Timestamp::from_secs(now),
                MAX_GRIND_ATTEMPTS,
            )
            .unwrap();

            assert_eq!(event.created_at, reference.created_at);
            assert!(event.compute_id() < reference.id);
            assert!(event.tags.iter().any(is_ngit_nonce));
        }
    }

    #[test]
    fn strictly_later_ordering_advances_timestamp_for_future_reference() {
        let keys = Keys::generate();
        let reference = reference_with_id(&"ff".repeat(32), u64::MAX - 1);

        let event = finalize_strictly_later_unsigned(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(u64::MAX));
        assert!(!event.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn strictly_later_timestamp_only_overrides_non_later_clock() {
        let reference = reference_with_id(&"ff".repeat(32), 10);

        assert_eq!(
            strictly_later_timestamp(Some(&reference), Timestamp::from_secs(10)).unwrap(),
            Some(Timestamp::from_secs(11))
        );
        assert_eq!(
            strictly_later_timestamp(Some(&reference), Timestamp::from_secs(11)).unwrap(),
            None
        );
    }

    #[test]
    fn infeasible_or_exhausted_grinding_uses_the_next_timestamp() {
        let keys = Keys::generate();
        let infeasible = reference_with_id(&"00".repeat(32), 10);
        let exhausted = reference_with_id(&"ff".repeat(32), 20);

        let after_infeasible = finalize_ordered_unsigned_at(
            candidate_builder(),
            keys.public_key(),
            Some(&infeasible),
            Timestamp::from_secs(10),
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap();
        let after_exhaustion = finalize_ordered_unsigned_at(
            candidate_builder(),
            keys.public_key(),
            Some(&exhausted),
            Timestamp::from_secs(20),
            0,
        )
        .unwrap();

        assert_eq!(MAX_GRIND_ATTEMPTS, 100_000);
        assert_eq!(after_infeasible.created_at, Timestamp::from_secs(11));
        assert_eq!(after_exhaustion.created_at, Timestamp::from_secs(21));
        assert!(!after_infeasible.tags.iter().any(is_ngit_nonce));
        assert!(!after_exhaustion.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn replaces_owned_nonce_and_retains_unrelated_nonce_tags() {
        let keys = Keys::generate();
        let reference = reference_with_id(&"00".repeat(32), 10);
        let other_nonce = Tag::parse(["nonce", "42", "0", "other-tool"]).unwrap();
        let event = finalize_ordered_unsigned_at(
            candidate_builder()
                .tag(ngit_nonce(123))
                .tag(other_nonce.clone()),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(10),
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap();

        assert!(event.tags.iter().any(|tag| tag == &other_nonce));
        assert!(!event.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn timestamp_overflow_is_an_error() {
        let keys = Keys::generate();
        let reference = reference_with_id(&"00".repeat(32), u64::MAX);

        let error = finalize_ordered_unsigned_at(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(u64::MAX),
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap_err();

        assert!(error.to_string().contains("timestamp overflow"));
    }
}
