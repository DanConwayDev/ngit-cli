//! Shared NIP-01 ordering policy for events that replace an earlier event.

use anyhow::{Result, bail};
use nostr::prelude::{
    Event, EventBuilder, EventId, PublicKey, Tag, Timestamp,
    event::{FinalizeUnsignedEvent, UnsignedEvent},
};

const MAX_EXPECTED_ATTEMPTS: u64 = 10_000;
// Minimum leading 64-bit prefix: an unconstrained replacement needs at most
// MAX_EXPECTED_ATTEMPTS hashes in expectation.
const SAFE_ID_FLOOR: u64 = u64::MAX / MAX_EXPECTED_ATTEMPTS + 1;
// A fresh timestamp reserves room for one guarded replacement within the
// expected-attempt budget. Same-timestamp chains can still consume that room.
const NEW_ID_FLOOR: u64 = SAFE_ID_FLOOR * 2;
const ISOLATED_ID_FLOOR: u64 = u64::MAX / 20 + 1;
const RECENT_ID_FLOOR: u64 = u64::MAX / 2 + 1;
const RECENT_UPDATE_SECONDS: u64 = 5;
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

/// The caller's ordering contract. There is deliberately no default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderingPolicy {
    /// Try a bounded lower-ID search, then advance past the reference.
    PreferSameTimestamp,
    /// Advance immediately, including beyond the wall clock; guard the new ID.
    StrictlyLater,
    /// Keep the semantic date; fail if a bounded lower-ID search cannot win.
    PreserveTimestamp(Timestamp),
}

/// Finalize an event according to an explicit ordering contract.
///
/// The returned candidate is unsigned and should be signed exactly once.
/// Timestamp advancement is checked for overflow and never waits for the clock.
pub fn finalize_ordered_unsigned(
    builder: EventBuilder,
    public_key: PublicKey,
    reference: Option<&Event>,
    policy: OrderingPolicy,
) -> Result<UnsignedEvent> {
    finalize_with_policy_at(
        builder,
        public_key,
        reference,
        policy,
        Timestamp::now(),
        MAX_GRIND_ATTEMPTS,
    )
}

fn finalize_with_policy_at(
    builder: EventBuilder,
    public_key: PublicKey,
    reference: Option<&Event>,
    policy: OrderingPolicy,
    now: Timestamp,
    max_grind_attempts: u128,
) -> Result<UnsignedEvent> {
    // Ordering owns the timestamp. A stale custom timestamp on a reused builder
    // must not bypass the policy when the clock already exceeds the reference.
    let builder = builder.custom_created_at(now);
    match policy {
        OrderingPolicy::PreferSameTimestamp => {
            finalize_ordered_unsigned_at(builder, public_key, reference, now, max_grind_attempts)
        }
        OrderingPolicy::StrictlyLater => {
            let mut builder = builder;
            builder.tags = nostr::prelude::Tags::from_list(
                builder
                    .tags
                    .into_iter()
                    .filter(|tag| !is_ngit_nonce(tag))
                    .collect(),
            );
            if let Some(created_at) = strictly_later_timestamp(reference, now)? {
                builder = builder.custom_created_at(created_at);
            }
            guard_new_id(
                builder,
                public_key,
                max_grind_attempts,
                fresh_id_floor(reference, now),
            )
        }
        OrderingPolicy::PreserveTimestamp(created_at) => {
            finalize_fixed_timestamp_ordered_unsigned_with_limit(
                builder,
                public_key,
                reference,
                created_at,
                fresh_id_floor(reference, now),
                max_grind_attempts,
            )
        }
    }
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
    builder.tags = nostr::prelude::Tags::from_list(
        builder
            .tags
            .into_iter()
            .filter(|tag| !is_ngit_nonce(tag))
            .collect(),
    );

    let preferred_floor = fresh_id_floor(reference, now);
    let Some(reference) = reference else {
        return guard_new_id(builder, public_key, max_grind_attempts, preferred_floor);
    };

    if now > reference.created_at {
        return guard_new_id(builder, public_key, max_grind_attempts, preferred_floor);
    }

    if let Some(candidate) = mine_replacement(&builder, public_key, reference, max_grind_attempts) {
        return Ok(candidate);
    }

    let created_at = reference
        .created_at
        .as_secs()
        .checked_add(1)
        .map(Timestamp::from_secs)
        .ok_or_else(|| anyhow::anyhow!("event timestamp overflow while ordering update"))?;
    guard_new_id(
        builder.custom_created_at(created_at),
        public_key,
        max_grind_attempts,
        preferred_floor,
    )
}

fn finalize_fixed_timestamp_ordered_unsigned_with_limit(
    mut builder: EventBuilder,
    public_key: PublicKey,
    reference: Option<&Event>,
    created_at: Timestamp,
    preferred_floor: u64,
    max_grind_attempts: u128,
) -> Result<UnsignedEvent> {
    builder.tags = nostr::prelude::Tags::from_list(
        builder
            .tags
            .into_iter()
            .filter(|tag| !is_ngit_nonce(tag))
            .collect(),
    );

    let Some(reference) = reference else {
        return guard_new_id(
            builder.custom_created_at(created_at),
            public_key,
            max_grind_attempts,
            preferred_floor,
        );
    };

    if created_at > reference.created_at {
        return guard_new_id(
            builder.custom_created_at(created_at),
            public_key,
            max_grind_attempts,
            preferred_floor,
        );
    }
    if created_at < reference.created_at {
        bail!(
            "fixed replacement timestamp {} predates current event timestamp {}",
            created_at.as_secs(),
            reference.created_at.as_secs()
        );
    }

    if let Some(candidate) = mine_replacement(&builder, public_key, reference, max_grind_attempts) {
        return Ok(candidate);
    }

    bail!("replacement ordering exhausted while preserving fixed timestamp")
}

pub(crate) fn is_ngit_nonce(tag: &Tag) -> bool {
    matches!(tag.as_slice(), [name, _, difficulty, marker] if name == "nonce" && difficulty == "0" && marker == NONCE_MARKER)
}

fn ngit_nonce(counter: u128) -> Tag {
    Tag::parse(["nonce", &counter.to_string(), "0", NONCE_MARKER]).expect("ngit nonce tag is valid")
}

fn leading_id(id: &EventId) -> u64 {
    u64::from_be_bytes(
        id.as_bytes()[..8]
            .try_into()
            .expect("event id has 32 bytes"),
    )
}

fn replacement_is_feasible(id: &EventId) -> bool {
    // Bound the expected cost of any winner, independently of its future cost.
    leading_id(id) >= SAFE_ID_FLOOR
}

fn mine_replacement(
    builder: &EventBuilder,
    public_key: PublicKey,
    reference: &Event,
    max_attempts: u128,
) -> Option<UnsignedEvent> {
    if !replacement_is_feasible(&reference.id) {
        return None;
    }
    // Prefer the highest 5% of winners, widening the range as inherited
    // difficulty grows to keep the preference within the expected-cost budget.
    // This preference must never discard a winner.
    let predecessor = leading_id(&reference.id);
    let preferred_floor = predecessor - (predecessor / 20).max(SAFE_ID_FLOOR);
    let mut best: Option<(EventId, UnsignedEvent)> = None;
    for nonce in 0..max_attempts {
        let candidate = builder
            .clone()
            .custom_created_at(reference.created_at)
            .tag(ngit_nonce(nonce))
            .finalize_unsigned(public_key);
        let id = candidate.compute_id();
        if id < reference.id {
            if leading_id(&id) >= preferred_floor {
                return Some(candidate);
            }
            if best.as_ref().is_none_or(|(best_id, _)| id > *best_id) {
                best = Some((id, candidate));
            }
        }
    }
    best.map(|(_, candidate)| candidate)
}

fn fresh_id_floor(reference: Option<&Event>, now: Timestamp) -> u64 {
    if reference.is_some_and(|event| {
        now.as_secs().saturating_sub(event.created_at.as_secs()) <= RECENT_UPDATE_SECONDS
    }) {
        RECENT_ID_FLOOR
    } else {
        ISOLATED_ID_FLOOR
    }
}

fn guard_new_id(
    builder: EventBuilder,
    public_key: PublicKey,
    max_attempts: u128,
    preferred_floor: u64,
) -> Result<UnsignedEvent> {
    guard_new_id_with_floor(
        builder,
        public_key,
        max_attempts,
        NEW_ID_FLOOR,
        preferred_floor,
    )
}

fn guard_new_id_with_floor(
    builder: EventBuilder,
    public_key: PublicKey,
    max_attempts: u128,
    floor: u64,
    preferred_floor: u64,
) -> Result<UnsignedEvent> {
    let candidate = builder.clone().finalize_unsigned(public_key);
    let id = candidate.compute_id();
    let preferred_floor = floor.max(preferred_floor);
    if leading_id(&id) >= preferred_floor {
        return Ok(candidate);
    }
    let mut best = (leading_id(&id) >= floor).then_some((id, candidate));
    for nonce in 0..max_attempts {
        let candidate = builder
            .clone()
            .tag(ngit_nonce(nonce))
            .finalize_unsigned(public_key);
        let id = candidate.compute_id();
        if leading_id(&id) >= preferred_floor {
            return Ok(candidate);
        }
        if leading_id(&id) >= floor && best.as_ref().is_none_or(|(best_id, _)| id > *best_id) {
            best = Some((id, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
        .ok_or_else(|| anyhow::anyhow!("event ID safety search exhausted"))
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{Keys, Kind, event::SignEvent};

    use super::*;

    #[test]
    fn recency_includes_five_seconds_and_future_predecessors() {
        let reference = reference_with_id(&"ff".repeat(32), 10);
        assert_eq!(
            fresh_id_floor(None, Timestamp::from_secs(10)),
            ISOLATED_ID_FLOOR
        );
        for now in [0, 9, 10, 14, 15] {
            assert_eq!(
                fresh_id_floor(Some(&reference), Timestamp::from_secs(now)),
                RECENT_ID_FLOOR
            );
        }
        for now in [16, u64::MAX] {
            assert_eq!(
                fresh_id_floor(Some(&reference), Timestamp::from_secs(now)),
                ISOLATED_ID_FLOOR
            );
        }
    }

    #[test]
    fn every_policy_keeps_isolated_ids_and_protects_recent_updates() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let builder = EventBuilder::new(Kind::TextNote, "fresh fallback");
        let original = builder
            .clone()
            .custom_created_at(Timestamp::from_secs(10))
            .finalize_unsigned(keys.public_key());
        assert!(leading_id(&original.compute_id()) >= ISOLATED_ID_FLOOR);
        assert!(leading_id(&original.compute_id()) < RECENT_ID_FLOOR);
        let recent = reference_with_id(&"00".repeat(32), 5);
        let old = reference_with_id(&"00".repeat(32), 4);
        for policy in [
            OrderingPolicy::StrictlyLater,
            OrderingPolicy::PreferSameTimestamp,
            OrderingPolicy::PreserveTimestamp(Timestamp::from_secs(10)),
        ] {
            for reference in [None, Some(&old), Some(&recent)] {
                let event = finalize_with_policy_at(
                    builder.clone(),
                    keys.public_key(),
                    reference,
                    policy,
                    Timestamp::from_secs(10),
                    100,
                )
                .unwrap();
                assert_eq!(event.created_at, original.created_at);
                if reference == Some(&recent) {
                    assert!(leading_id(&event.compute_id()) >= RECENT_ID_FLOOR);
                    assert_ne!(event.compute_id(), original.compute_id());
                } else {
                    assert_eq!(event.compute_id(), original.compute_id());
                    assert!(!event.tags.iter().any(is_ngit_nonce));
                }
            }
        }
        // A release's semantic date must not stand in for the observed clock.
        let event = finalize_with_policy_at(
            builder,
            keys.public_key(),
            Some(&recent),
            OrderingPolicy::PreserveTimestamp(Timestamp::from_secs(10)),
            Timestamp::from_secs(100),
            100,
        )
        .unwrap();
        assert_eq!(event.compute_id(), original.compute_id());
    }

    #[test]
    fn every_policy_guards_a_known_low_id_at_a_fresh_timestamp() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        // Precomputed fixture; no random predecessor or fixture mining.
        let builder = EventBuilder::new(Kind::TextNote, "guard fixture 14073")
            .custom_created_at(Timestamp::from_secs(10));
        assert_eq!(
            builder
                .clone()
                .finalize_unsigned(keys.public_key())
                .compute_id()
                .to_hex(),
            "00023fb05a7ffb367c20d746399e58de9c8127df3356055844e6bf2a164e7ca0"
        );
        let older = reference_with_id(&"00".repeat(32), 9);
        for policy in [
            OrderingPolicy::StrictlyLater,
            OrderingPolicy::PreferSameTimestamp,
            OrderingPolicy::PreserveTimestamp(Timestamp::from_secs(10)),
        ] {
            for (reference, now) in [(None, 10), (Some(&older), 10), (Some(&older), 8)] {
                let guarded = finalize_with_policy_at(
                    builder.clone(),
                    keys.public_key(),
                    reference,
                    policy,
                    Timestamp::from_secs(now),
                    MAX_GRIND_ATTEMPTS,
                )
                .unwrap();
                assert_eq!(guarded.created_at, Timestamp::from_secs(10));
                assert!(
                    leading_id(&guarded.compute_id())
                        >= fresh_id_floor(reference, Timestamp::from_secs(now))
                );
                assert!(guarded.tags.iter().any(is_ngit_nonce));
            }
        }
    }

    #[test]
    fn same_timestamp_search_prefers_a_modest_difficulty_increase() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let builder = EventBuilder::new(Kind::TextNote, "guard fixture 8366")
            .custom_created_at(Timestamp::from_secs(10));
        let unsafe_id = builder
            .clone()
            .tag(ngit_nonce(0))
            .finalize_unsigned(keys.public_key())
            .compute_id();
        assert_eq!(
            unsafe_id.to_hex(),
            "0003933b677343eda6bc4fd2ac35b6c36d0734eac692194eb4767d42bd98e737"
        );
        let reference = reference_with_id(&"ff".repeat(32), 10);
        for policy in [
            OrderingPolicy::PreferSameTimestamp,
            OrderingPolicy::PreserveTimestamp(Timestamp::from_secs(10)),
        ] {
            let guarded = finalize_with_policy_at(
                builder.clone(),
                keys.public_key(),
                Some(&reference),
                policy,
                Timestamp::from_secs(10),
                MAX_GRIND_ATTEMPTS,
            )
            .unwrap();
            assert_eq!(guarded.created_at, reference.created_at);
            assert!(guarded.compute_id() < reference.id);
            assert!(
                leading_id(&guarded.compute_id())
                    >= leading_id(&reference.id) - leading_id(&reference.id) / 20
            );
            assert_ne!(guarded.compute_id(), unsafe_id);
        }
    }

    #[test]
    fn difficult_predecessor_relaxes_the_quality_preference() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let builder = EventBuilder::new(Kind::TextNote, "guard fixture 8366");
        let reference =
            reference_with_id(&format!("{:016x}{}", SAFE_ID_FLOOR, "00".repeat(24)), 10);
        let winner = builder
            .clone()
            .custom_created_at(reference.created_at)
            .tag(ngit_nonce(0))
            .finalize_unsigned(keys.public_key())
            .compute_id();
        // At the feasibility limit even a large relative ID drop qualifies.
        assert!(leading_id(&winner) < leading_id(&reference.id) * 19 / 20);
        assert!(winner < reference.id);
        let event = mine_replacement(&builder, keys.public_key(), &reference, 2).unwrap();
        assert_eq!(event.compute_id(), winner);
    }

    #[test]
    fn inherited_difficulty_does_not_discard_a_valid_replacement() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let builder = EventBuilder::new(Kind::TextNote, "guard fixture 8366");
        // This legacy predecessor is eligible but below the fresh-event floor.
        let reference = reference_with_id(
            &format!("{:016x}{}", SAFE_ID_FLOOR * 7 / 4, "00".repeat(24)),
            10,
        );
        let winner = builder
            .clone()
            .custom_created_at(reference.created_at)
            .tag(ngit_nonce(0))
            .finalize_unsigned(keys.public_key())
            .compute_id();
        assert!(winner < reference.id);
        assert!(leading_id(&winner) < leading_id(&reference.id) - SAFE_ID_FLOOR);
        for policy in [
            OrderingPolicy::PreferSameTimestamp,
            OrderingPolicy::PreserveTimestamp(reference.created_at),
        ] {
            let event = finalize_with_policy_at(
                builder.clone(),
                keys.public_key(),
                Some(&reference),
                policy,
                reference.created_at,
                1,
            )
            .unwrap();
            assert_eq!(event.created_at, reference.created_at);
            assert_eq!(event.compute_id(), winner);
        }
    }

    #[test]
    fn fresh_id_preference_keeps_a_safe_candidate_when_budget_is_exhausted() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let builder = EventBuilder::new(Kind::TextNote, "fresh fallback")
            .custom_created_at(Timestamp::from_secs(10));
        let original = builder.clone().finalize_unsigned(keys.public_key());
        assert!(leading_id(&original.compute_id()) >= NEW_ID_FLOOR);
        assert!(leading_id(&original.compute_id()) < RECENT_ID_FLOOR);
        let event = guard_new_id(builder, keys.public_key(), 0, RECENT_ID_FLOOR).unwrap();
        assert_eq!(event.compute_id(), original.compute_id());
        assert_eq!(event.created_at, original.created_at);
    }

    #[test]
    fn id_guard_retries_before_signing_without_changing_timestamp() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let builder = candidate_builder().custom_created_at(Timestamp::from_secs(10));
        let original = builder.clone().finalize_unsigned(keys.public_key());
        let floor = leading_id(&original.compute_id()).checked_add(1).unwrap();
        let guarded =
            guard_new_id_with_floor(builder.clone(), keys.public_key(), 100_000, floor, floor)
                .unwrap();
        assert!(leading_id(&guarded.compute_id()) >= floor);
        assert_eq!(guarded.created_at, original.created_at);
        assert_eq!(guarded.content, original.content);
        assert!(guarded.tags.iter().any(is_ngit_nonce));
        assert!(
            guard_new_id_with_floor(builder, keys.public_key(), 0, floor, floor)
                .unwrap_err()
                .to_string()
                .contains("safety search exhausted")
        );
    }

    #[test]
    fn same_timestamp_feasibility_uses_raw_replacement_cost() {
        let narrow = reference_with_id(
            &format!("{:016x}{}", SAFE_ID_FLOOR - 1, "00".repeat(24)),
            10,
        );
        let enough = reference_with_id(&format!("{:016x}{}", SAFE_ID_FLOOR, "00".repeat(24)), 10);
        assert!(!replacement_is_feasible(&narrow.id));
        assert!(replacement_is_feasible(&enough.id));
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let advanced = finalize_with_policy_at(
            candidate_builder(),
            keys.public_key(),
            Some(&narrow),
            OrderingPolicy::PreferSameTimestamp,
            Timestamp::from_secs(10),
            100_000,
        )
        .unwrap();
        assert_eq!(advanced.created_at, Timestamp::from_secs(11));
        assert!(leading_id(&advanced.compute_id()) >= SAFE_ID_FLOOR);
        assert!(
            finalize_with_policy_at(
                candidate_builder(),
                keys.public_key(),
                Some(&narrow),
                OrderingPolicy::PreserveTimestamp(Timestamp::from_secs(10)),
                Timestamp::from_secs(10),
                100_000
            )
            .is_err()
        );
    }

    #[test]
    fn policy_overrides_stale_builder_timestamp_with_the_observed_clock() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"00".repeat(32), 5);
        for policy in [
            OrderingPolicy::StrictlyLater,
            OrderingPolicy::PreferSameTimestamp,
        ] {
            for reference in [None, Some(&reference)] {
                let unsigned = finalize_with_policy_at(
                    candidate_builder().custom_created_at(Timestamp::from_secs(1)),
                    keys.public_key(),
                    reference,
                    policy,
                    Timestamp::from_secs(10),
                    0,
                )
                .unwrap();
                assert_eq!(unsigned.created_at, Timestamp::from_secs(10));
            }
        }
    }

    #[test]
    fn explicit_policies_distinguish_advancement_from_fixed_date_exhaustion() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"00".repeat(32), 20);
        for now in [10, 20] {
            for policy in [
                OrderingPolicy::PreferSameTimestamp,
                OrderingPolicy::StrictlyLater,
                OrderingPolicy::PreserveTimestamp(reference.created_at),
            ] {
                let result = finalize_with_policy_at(
                    candidate_builder(),
                    keys.public_key(),
                    Some(&reference),
                    policy,
                    Timestamp::from_secs(now),
                    0,
                );
                match policy {
                    OrderingPolicy::PreserveTimestamp(_) => {
                        assert!(
                            result
                                .unwrap_err()
                                .to_string()
                                .contains("ordering exhausted")
                        );
                    }
                    _ => {
                        let event = result.unwrap();
                        assert_eq!(event.created_at, Timestamp::from_secs(21));
                        assert!(!event.tags.iter().any(is_ngit_nonce));
                    }
                }
            }
        }
    }

    #[test]
    fn advancing_policies_reject_overflow_without_wrapping() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"00".repeat(32), u64::MAX);
        for policy in [
            OrderingPolicy::PreferSameTimestamp,
            OrderingPolicy::StrictlyLater,
        ] {
            let error = finalize_with_policy_at(
                candidate_builder(),
                keys.public_key(),
                Some(&reference),
                policy,
                Timestamp::from_secs(10),
                0,
            )
            .unwrap_err();
            assert!(error.to_string().contains("timestamp overflow"));
        }
    }

    #[test]
    fn latest_event_prefers_lower_id_on_timestamp_tie() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let event = finalize_ordered_unsigned_at(
            candidate_builder().custom_created_at(Timestamp::from_secs(7)),
            keys.public_key(),
            None,
            Timestamp::from_secs(10),
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(7));
        assert!(leading_id(&event.compute_id()) >= ISOLATED_ID_FLOOR);
    }

    #[test]
    fn older_reference_preserves_the_builder_timestamp() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
        assert!(leading_id(&event.compute_id()) >= RECENT_ID_FLOOR);
    }

    #[test]
    fn equal_and_future_feasible_references_are_beaten_at_the_same_timestamp() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
            assert!(leading_id(&event.compute_id()) >= SAFE_ID_FLOOR);
            assert!(event.tags.iter().any(is_ngit_nonce));
        }
    }

    #[test]
    fn strictly_later_ordering_advances_timestamp_for_future_reference() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"ff".repeat(32), u64::MAX - 1);

        let event = finalize_ordered_unsigned(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            OrderingPolicy::StrictlyLater,
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(u64::MAX));
        assert!(leading_id(&event.compute_id()) >= RECENT_ID_FLOOR);
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
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
        assert!(leading_id(&after_infeasible.compute_id()) >= RECENT_ID_FLOOR);
        assert!(!after_exhaustion.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn fixed_timestamp_grinds_without_changing_the_domain_date() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"ff".repeat(32), 20);

        let event = finalize_fixed_timestamp_ordered_unsigned_with_limit(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(20),
            RECENT_ID_FLOOR,
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(20));
        assert!(event.compute_id() < reference.id);
        assert!(event.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn fixed_timestamp_accepts_an_explicitly_later_date() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"00".repeat(32), 20);

        let event = finalize_fixed_timestamp_ordered_unsigned_with_limit(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(21),
            RECENT_ID_FLOOR,
            0,
        )
        .unwrap();

        assert_eq!(event.created_at, Timestamp::from_secs(21));
        assert!(!event.tags.iter().any(is_ngit_nonce));
    }

    #[test]
    fn fixed_timestamp_rejects_older_or_unbeatable_dates() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"ff".repeat(32), 20);

        let older = finalize_fixed_timestamp_ordered_unsigned_with_limit(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(19),
            RECENT_ID_FLOOR,
            MAX_GRIND_ATTEMPTS,
        )
        .unwrap_err();
        let exhausted = finalize_fixed_timestamp_ordered_unsigned_with_limit(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(20),
            RECENT_ID_FLOOR,
            0,
        )
        .unwrap_err();

        assert!(older.to_string().contains("predates current event"));
        assert!(exhausted.to_string().contains("ordering exhausted"));
    }

    #[test]
    fn fixed_timestamp_rejects_the_low_release_id_observed_in_ci() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(
            "00015ade2e7a1720a57a98c9522100919ab8c7d3d95764f6fd789f7e1361af32",
            1_789_380_943,
        );
        // This predecessor requires about 48,368 expected attempts, beyond
        // the 10,000 feasibility ceiling, regardless of the candidate.
        assert!(!replacement_is_feasible(&reference.id));
        let error = finalize_ordered_unsigned(
            candidate_builder(),
            keys.public_key(),
            Some(&reference),
            OrderingPolicy::PreserveTimestamp(reference.created_at),
        )
        .unwrap_err();
        assert!(error.to_string().contains("ordering exhausted"));
    }

    #[test]
    fn fixed_timestamp_replaces_only_the_owned_nonce() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
        let reference = reference_with_id(&"00".repeat(32), 20);
        let other_nonce = Tag::parse(["nonce", "42", "0", "other-tool"]).unwrap();

        let event = finalize_fixed_timestamp_ordered_unsigned_with_limit(
            candidate_builder()
                .tag(ngit_nonce(123))
                .tag(other_nonce.clone()),
            keys.public_key(),
            Some(&reference),
            Timestamp::from_secs(21),
            RECENT_ID_FLOOR,
            0,
        )
        .unwrap();

        assert!(event.tags.iter().any(|tag| tag == &other_nonce));
        assert!(!event.tags.iter().any(|tag| tag == &ngit_nonce(123)));
        assert!(event.tags.iter().filter(|tag| is_ngit_nonce(tag)).count() <= 1);
    }

    #[test]
    fn replaces_owned_nonce_and_retains_unrelated_nonce_tags() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
        assert!(!event.tags.iter().any(|tag| tag == &ngit_nonce(123)));
        assert!(event.tags.iter().filter(|tag| is_ngit_nonce(tag)).count() <= 1);
    }

    #[test]
    fn timestamp_overflow_is_an_error() {
        let keys = Keys::parse(&"01".repeat(32)).unwrap();
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
