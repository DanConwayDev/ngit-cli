# Event `created_at` Ordering

Status: proposed; implementation requires maintainer approval.

## Problem

Nostr timestamps have one-second resolution. When ngit publishes a new version
of an event in the same second as the version it is intended to supersede, both
events have the same `created_at`. Consumers and relays then use the event ID as
a tiebreaker. NIP-01 gives the lower event ID precedence. Because the new event
ID is not predictably ordered before the old one, the update has roughly a 50%
chance of losing.

This can also happen when an existing event has a future timestamp. Repeatedly
publishing with the current wall-clock timestamp cannot supersede that event
until the clock catches up.

## Scope

Apply explicit timestamp ordering to:

- repository state events (kind 30618),
- repository announcement events (kind 30617),
- pull request update events (kind 1619), and
- proposal status events (kinds 1630 through 1633).

Do not apply it to patch events. Patches use NIP-10 threading to establish
revision order rather than selecting a revision by timestamp.

Do not change initial pull request event timestamps. Only a pull request update
needs to be ordered after an existing pull request history.

## Reference Event

Before building an affected event, identify the event that the new event must
sort after:

- **State:** the canonical latest state event for the repository identifier
  across the maintainer set.
- **Announcement:** the latest announcement event for the repository identifier
  across the maintainer set. This preserves the existing rule that shared
  metadata comes from the latest maintainer announcement.
- **Pull request update:** the newest event by `created_at` among the root pull
  request and all previous pull request updates for that pull request. Both the
  root PR timestamp and the latest update timestamp must therefore be
  considered.
- **Proposal status:** the latest status event for the same proposal thread,
  regardless of which status kind it uses.

When events tie on `created_at` while selecting the reference, the event with
the lowest event ID wins, as required by NIP-01. Retain the winning event, not
only its timestamp, because nonce feasibility requires the full event ID.

Some existing ngit selection paths currently choose the greater event ID on a
timestamp tie. The implementation must correct those paths to choose the lower
ID before relying on nonce grinding; otherwise ngit and NIP-01-compliant relays
would disagree about the winner.

Only events already fetched into the local cache can be considered. Existing
fetch-before-publish flows remain responsible for making that cache current.

## Update Strategy

Let:

- `now` be the current Unix timestamp immediately before timestamp selection,
  and
- `reference_created_at` be the reference event's timestamp, when one exists.

Use the normal `EventBuilder::new(...)` timestamp when there is no reference, or
when `now > reference_created_at`.

When `now <= reference_created_at`, first evaluate whether it is practical to
create a lower event ID at the reference timestamp by varying an ngit-owned
nonce tag. If grinding is practical and finds a winner, use:

```text
created_at = reference_created_at
event_id < reference_event_id
```

If grinding is not practical or exhausts its bounded attempt budget, fall back
to:

```text
created_at = reference_created_at + 1 second
```

and construct the event with:

```rust
EventBuilder::new(kind, content).custom_created_at(created_at)
```

The fallback addition must be checked; timestamp overflow is an error rather
than a wraparound.

Timestamp selection must happen before the event is finalized or signed. For a
PR update it must also happen before its event ID is used to form a
`refs/nostr/<event-id>` git ref.

## Nonce Grinding

Event IDs are SHA-256 hashes and can be treated as uniformly distributed over
the 256-bit ID space. For ngit's lower-ID-wins ordering, the probability that
one candidate beats the reference is approximately:

```text
p = reference_event_id / 2^256
expected_attempts = 1 / p
```

For a typical reference ID, the expected work is two attempts. References very
close to zero are expensive, so grinding must be bounded rather than assumed to
succeed.

The proposed bounds are:

- grind only when the reference ID implies at most 10,000 expected attempts,
- try at most 100,000 nonce values, and
- fall back to `reference_created_at + 1` if no candidate wins within that
  budget.

At the feasibility boundary, 100,000 attempts provide approximately a 99.995%
chance of success. The expected-attempt estimate can use the most-significant
64 bits of the event ID; this is sufficient for the feasibility cutoff and
avoids adding a big-integer dependency.

Finalize an unsigned candidate at `reference_created_at`, compute its ID, and
vary only this tag until `candidate_id < reference_event_id`:

```text
["nonce", "<u128 counter>", "0", "ngit-created-at-tiebreak"]
```

The tag is a NIP-13-compatible nonce with zero claimed proof-of-work difficulty.
The fourth value identifies it as ngit-owned. Remove an existing tag with this
exact marker before generating candidates so repeated announcement updates do
not accumulate grinding tags. Preserve unrelated or third-party nonce tags.

ID calculation does not require a signature, so grinding happens entirely on
the unsigned event. Sign only the winning candidate. This keeps local and
remote signer behavior identical and avoids repeated remote-signing requests.

Future-dated reference events use exactly the same decision. Grind at the
reference's future timestamp when feasible; otherwise use
`reference_created_at + 1`. Do not wait for wall-clock time to catch up and do
not add countdown or explanatory output.

## Proposed Implementation Shape

Add one shared helper that receives an unsigned event builder and the optional
reference event. It decides whether to use the normal timestamp, grind at the
reference timestamp, or use the `reference + 1` fallback, and returns the
unsigned event to sign. Keeping policy in one helper avoids different bounds or
timestamp rules across state, announcement, PR update, and status paths.

This is practical with the current code and nostr dependency:

- `EventBuilder` is cloneable, so each nonce can finalize an independent
  unsigned candidate.
- `UnsignedEvent::compute_id()` derives each candidate ID without signing.
- `RepoState::event` and the push rollback state retain full existing state
  events.
- `RepoRef::events` retains full announcement events for every maintainer.
- proposal discovery already loads the root PR and its update events. The PR
  generation API currently receives only the root, so it must additionally pass
  the winning previous update.

Callers remain responsible for selecting the correct reference event:

- `RepoState::build` callers provide the canonical state event being replaced.
- `RepoRef::to_event` callers provide the latest announcement across
  maintainers.
- PR update generation provides the root PR and previous updates, then selects
  the maximum timestamp before finalizing the unsigned event.
- status generation provides the latest status event for the proposal thread.

All production paths that create these event types must use the policy,
including `ngit init`, `ngit sync`, maintainer acceptance, normal nostr pushes,
automatic maintainer-announcement publication, and status changes from PR and
merge commands.

## Test Plan

Nonce tests should inject deterministic candidate IDs rather than depend on
random SHA-256 outcomes. Cover:

1. no reference event uses the builder's normal timestamp,
2. a reference older than `now` uses the builder's normal timestamp,
3. a feasible reference equal to `now` is beaten by a lower candidate ID at the
   same timestamp,
4. a feasible future reference is beaten at the same timestamp,
5. infeasible grinding selects `reference + 1`,
6. exhausting 100,000 candidates selects `reference + 1`,
7. grinding stops at the first lower candidate and signs only once,
8. an old ngit-owned nonce is replaced while unrelated nonce tags are retained,
9. equal timestamps consistently select the lower ID throughout ngit,
10. a future reference is handled without sleeping or user messaging,
11. checked timestamp overflow returns an error,
12. PR updates compare both the root PR and all previous PR updates, and
13. status updates compare every status kind in the same proposal thread.

Integration tests should verify observable relay state for rapid consecutive
state, announcement, PR-update, and proposal-status publications. Patch
publication should have a regression test confirming that its NIP-10 ordering
and timestamp behavior are unchanged.

The test harness's existing one-second pre-push pacing may be removed from
production-covered state publication scenarios once those scenarios prove the
new ordering behavior. Direct fixture publication still needs explicit
timestamps or pacing when it intentionally bypasses ngit's event builders.

## Acceptance Criteria

- Every affected new event either has the same `created_at` and a lower event ID
  than the reference, or has a strictly greater `created_at`.
- Feasible timestamp ties are resolved by bounded nonce grinding before using
  `reference_created_at + 1`.
- All ngit event-selection paths agree with NIP-01 that the lower ID wins a
  timestamp tie.
- Future-dated references use the same bounded-grind-or-increment policy without
  waiting or additional user messaging.
- Patches and initial PR events retain their current timestamp behavior.
- Local and remote signers follow the same ordering policy.

## Implementation Agent Prompt

```text
Implement the event created_at ordering design in
docs/architecture/event-created-at-ordering.md. Read that document and the
repository AGENTS.md before changing code, and treat the document's scope,
algorithm, grinding bounds, nonce-tag format, and acceptance criteria as the
source of truth.

Apply the shared policy to repository state events (30618), repository
announcements (30617), PR updates (1619), and proposal status events
(1630..=1633). Do not apply it to patches or initial PR events. Correct existing
timestamp-tie comparisons so the lower event ID wins consistently with NIP-01.
For PR updates, compare the root PR and every previous PR update. For statuses,
compare all status kinds in the proposal thread. Future-dated references must
use the same grind-or-increment policy without sleeping or printing additional
messages.

Keep the policy centralized, preserve unrelated nonce tags, sign only the final
candidate, and avoid adding dependencies unless strictly necessary. Add the
unit and integration coverage listed in the design. Follow the test-harness
boundary rules: no serial tests, process-global environment mutation, PTY
interaction, or exact-stdout assertions. Run cargo fmt, targeted tests, the full
test suite, and cargo clippy. Fix all failures and warnings, then summarize the
implementation with file:line references and the verification performed.
```
