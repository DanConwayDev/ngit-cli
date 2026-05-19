//! Wall-clock pacing helpers used to keep nostr `created_at` timestamps unique.
//!
//! Nostr `created_at` is unix-seconds (NIP-01) — second resolution. Two events
//! signed by the same key, with the same kind, tags and content, signed in the
//! same wall-clock second, will hash to the **same event id**. For replaceable
//! / addressable events (kinds 10000–19999 and 30000–39999), that's nominally
//! fine — they're the same event. But `nostr-relay-builder`'s in-memory
//! database has a quirk worth knowing about: when a newer replaceable event
//! supersedes an older one at the same coordinate, the older event's id is
//! added to the database's `deleted_ids` set (see
//! `nostr-database/src/helper.rs::discard_events`). Any subsequent attempt to
//! save an event whose id is already in `deleted_ids` is rejected via
//! `check_id` with the wire message `"blocked: this event is deleted"` — even
//! though no NIP-09 deletion ever happened.
//!
//! Combined with second-resolution timestamps that's a flake recipe: tests
//! that publish two state events back-to-back with identical content, fast
//! enough that they share a `created_at` second AND the same hashmap
//! iteration order, hit it directly; tests that publish one explicit state
//! event then a follow-up state event whose `(content, created_at)` happens
//! to collide with an *auto*-generated state event from an intervening
//! `git push` hit it indirectly. Both surfaced in
//! `tests/list_state.rs::state_event_takes_precedence_over_advanced_git_server_state`
//! at roughly 30% flake rate on fast hardware.
//!
//! The fix at this layer is unconditional: every harness operation that
//! publishes a nostr event (either directly via [`Harness::publish_state_event`]
//! or indirectly via a `git push` to a nostr remote) ends by waiting for the
//! wall clock to roll into the next whole unix second. The next caller's
//! `Timestamp::now()` is then guaranteed to land in a strictly later second
//! than the event just published, so created-at-based event-id collisions are
//! impossible by construction.
//!
//! A flat one-second sleep was chosen over a poll-loop for predictability:
//! every push or publish in the harness costs ~1s of wall time. Migrated
//! `tests/list_state.rs` runs go from ~1.2s (with intermittent failure) to
//! ~5s reliably — an acceptable trade for hermetic timing semantics.

use std::time::Duration;

/// Sleep one whole second, regardless of where in the current second the
/// caller currently is.
///
/// This is the bluntest possible implementation — see the module-level
/// rationale for why a flat sleep beats a poll-and-wake against `Timestamp::now`
/// in this codebase. Designed to be called from harness helpers immediately
/// after a successful nostr publish, with the contract that any subsequent
/// `Timestamp::now()` lands in a strictly later unix second than the event
/// just published.
///
/// Callers must be inside a tokio runtime (the harness is async-first; every
/// existing call site already is).
pub async fn tick_to_next_second() {
    tokio::time::sleep(Duration::from_secs(1)).await;
}
