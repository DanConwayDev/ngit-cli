//! CI status and trust context.
//!
//! Consumes the CI events defined by the ngit-ci NIP (kinds 9840-9844 and
//! 39842) and labels every result with the CI trust-context model so a
//! signature is never presented as maintainer endorsement.
//!
//! - [`kinds`] — kind constants and strict shape validation.
//! - [`events`] — workflow-run grouping and run state.
//! - [`controls`] — Service Request/Stop reduction and coordinator relationship
//!   tiers.
//! - [`trust`] — evidence types, classification, run/job resolution, rollups
//!   and the canonical wording shared with gitworkshop.
//! - [`provenance`] — validation of a run's frozen request quote.
//! - [`domain`] — verified NIP-05 identities and the repository-domain ladder.
//! - [`resolve`] — cache-tier and full-tier assembly of the trust context.
//!
//! Every time-dependent function takes an explicit `now`/`at` [`Timestamp`]
//! rather than reading the system clock, so the semantics are unit testable.
//!
//! See `docs/architecture/ci-trust.md`.

pub mod controls;
pub mod domain;
pub mod events;
pub mod kinds;
pub mod provenance;
pub mod resolve;
pub mod trust;

use std::cmp::Reverse;

use nostr::prelude::{EventId, Timestamp};

/// Position in the total order the NIP defines for immutable CI events: a
/// greater `created_at` is later, and at equal timestamps the
/// lexicographically lower event id is later.
///
/// The returned tuple orders with `Ord`, so `position(a) > position(b)` means
/// `a` is later than `b`.
#[must_use]
pub fn total_order_position(
    created_at: Timestamp,
    event_id: EventId,
) -> (Timestamp, Reverse<EventId>) {
    (created_at, Reverse(event_id))
}
