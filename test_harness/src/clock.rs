//! Wall-clock pacing helpers used to keep nostr `created_at` timestamps
//! unique across same-coordinate replaceable-event publishes.
//!
//! # The collision
//!
//! Nostr `created_at` is unix-seconds (NIP-01) — second resolution. Two
//! events signed by the same key, with the same kind, tags and content,
//! signed in the same wall-clock second, hash to the **same event id**.
//! For replaceable / addressable events (kinds 10000–19999 and
//! 30000–39999) `nostr-relay-builder`'s in-memory database has a quirk
//! worth knowing about: when a newer replaceable event supersedes an
//! older one at the same `(pubkey, kind, d-tag)` coordinate, the older
//! event's id is added to the database's `deleted_ids` set (see
//! `nostr-database/src/helper.rs::discard_events`). Any subsequent
//! attempt to save an event whose id is already in `deleted_ids` is
//! rejected via `check_id` with the wire message
//! `"blocked: this event is deleted"` — even though no NIP-09 deletion
//! ever happened.
//!
//! So the property the harness actually needs is narrow: **two
//! same-coordinate replaceable events with identical
//! `(pubkey, kind, tags, content)` must not share a `created_at`
//! second.** Non-replaceable events (patches, status events, ephemeral
//! signer-connect events, etc.) don't trigger `deleted_ids` and don't
//! need pacing — collisions there are nominal because nothing
//! supersedes anything.
//!
//! # The discipline: tick *before* publishing
//!
//! Every harness operation that publishes a replaceable event ticks the
//! wall clock into a fresh second **before** building/signing the
//! event. The contract is local to the publisher: "I, the publisher of
//! a replaceable event, ensure I'm in a fresh second before I publish —
//! I don't rely on whoever ran before me having cleaned up." Two
//! consequences:
//!
//! - The tick belongs in front of the publish-side code path, not after it. The
//!   safety property is about *this* event's `created_at`, not the previous
//!   one's. A tick-after design only works for the next helper-driven publish;
//!   any bare `Client::send_event_to` in test code that runs in between still
//!   risks colliding with the downstream helper.
//! - Non-replaceable publishers do not tick. Patches (kind-1617), issues
//!   (kind-1621), status events (kind-1630..1633), signer-connect (kind-24134),
//!   etc. can publish at full speed.
//!
//! Concrete harness sites that tick before publishing today:
//!
//! - [`crate::repo::Repo::nostr_push`] — runs `git push` against a `nostr://`
//!   remote; the `git-remote-nostr` helper subprocess internally builds and
//!   signs a kind-30618 state event during the push. The tick happens before
//!   the `git push` invocation so the helper's `Timestamp::now()` lands in the
//!   fresh second.
//! - [`crate::harness::Harness::publish_state_event`] — explicit kind-30618
//!   publish. Ticks before `EventBuilder::new(...)` / `sign_with_keys(...)` so
//!   the freshly-signed event's `created_at` is the post-tick second. (Skipped
//!   when the caller asks for a back-dated event via `created_at_offset_secs` —
//!   the whole point of that knob is to produce a deterministically-older
//!   timestamp.)
//!
//! # The flat one-second sleep
//!
//! [`tick_to_next_second`] is a flat 1s `tokio::time::sleep`. Predictable
//! over a poll-and-wake against `Timestamp::now`: every replaceable
//! publish in the harness costs ~1s of wall time, no more, no less. The
//! migrated `tests/list_state.rs` runs go from ~1.2s (with intermittent
//! failure under the old tick-after design) to ~5s reliably under
//! tick-before — an acceptable trade for hermetic timing semantics.

use std::time::Duration;

/// Sleep one whole second, regardless of where in the current second the
/// caller currently is.
///
/// Designed to be called from a harness helper that is about to publish
/// a replaceable nostr event (kinds 10000–19999, 30000–39999), with the
/// contract that the subsequent `Timestamp::now()` taken when the event
/// is built/signed lands in a strictly later unix second than any prior
/// same-coordinate replaceable publish. See the module-level docs for
/// why a flat sleep beats a poll loop in this codebase, and why
/// non-replaceable publishers don't call this at all.
///
/// Callers must be inside a tokio runtime (the harness is async-first;
/// every existing call site already is).
pub async fn tick_to_next_second() {
    tokio::time::sleep(Duration::from_secs(1)).await;
}
