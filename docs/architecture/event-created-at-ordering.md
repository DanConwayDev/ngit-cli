# Event `created_at` Ordering

Status: implemented.

## Problem

Nostr timestamps have one-second resolution. If ngit publishes a replacement
event in the same second as the event it supersedes, relays and consumers use
the event ID as a tiebreaker. Under NIP-01, the lower ID wins. A normally
created replacement therefore has an unpredictable chance of losing to the
older event.

Proposal history has a related constraint. ngit identifies the active patch or
pull-request revision by its newest timestamp and then follows that revision's
thread. Distinct revisions with the same timestamp are ambiguous even though
their events are not NIP-01 replaceable events.

The same problem occurs when the current event is future-dated: publishing at
the wall-clock time cannot replace it until the clock catches up.

## Scope

ngit applies explicit ordering to repository and proposal metadata:

- repository state events (kind 30618),
- repository announcements (kind 30617),
- patch revision series (kind 1617),
- pull-request upgrades and updates (kinds 1618 and 1619), and
- proposal statuses (kinds 1630 through 1633).

Initial patch and pull-request proposals retain the current timestamp. A patch
appended to an existing series also relies on its NIP-10 parent link. Explicit
proposal-history ordering applies when publishing a new patch revision, a
patch-to-PR upgrade, or a PR update that supersedes the active proposal tip.

## Ordering policy

Before publishing an affected event, ngit identifies the event it must follow:

- state and announcement updates use the canonical latest event for the
  repository across maintainers;
- a proposal revision or update considers its root and prior proposal events;
  and
- a proposal status considers all status kinds in the same proposal thread.

In every case, the latest `created_at` wins. For equal timestamps, the lowest
event ID wins, matching NIP-01. Only events already present in the local cache
can be considered, so normal fetch-before-publish flows remain important.

Single-event finalization uses `finalize_ordered_unsigned` with a required
`OrderingPolicy`; there is no implicit default:

| Policy | Callers | Same-second or future predecessor |
| --- | --- | --- |
| `PreferSameTimestamp` | State, announcements, statuses, containers, software applications/assets | Bounded lower-ID search, then checked timestamp advancement |
| `StrictlyLater` | PR upgrades/updates, private Git relay lists | Checked timestamp advancement without mining |
| `PreserveTimestamp(date)` | Release edits | Bounded lower-ID search at the explicit date; exhaustion is an error |

Patch series use the shared `strictly_later_timestamp` calculation once to keep
every event in one revision on the same timestamp. It implements the same
strict advancement rule as `StrictlyLater`.

Release dates are domain metadata as well as event timestamps. An explicit
older date is rejected; an explicitly newer date needs no mining. Fixed-date
exhaustion must not silently change the release date.

Nsites currently wait for an observed later wall-clock second. This is separate
from the immediate timestamp advancement used by `StrictlyLater`.

Readers still use the canonical latest timestamp and lower-ID tie-break.
Strict revision timestamps also protect interoperability with clients that
do not resolve timestamp ties correctly.


### NIP-01 replacement ordering

Repository state, repository announcements, and proposal statuses use NIP-01's
event-ID tiebreak. GRASP now applies that same ordering when promoting state
events from purgatory, including retaining the lower event ID when timestamps
tie. If the current timestamp is not newer than the reference, ngit tries to
publish at the reference timestamp with a lower event ID by varying an
ngit-owned nonce tag. This is bounded: ngit only attempts grinding when the
reference ID makes it practical, and stops after a finite attempt budget.

If grinding is unsuitable or does not produce a lower ID, ngit publishes at one
second after the reference timestamp. The addition is checked, so timestamp
overflow is reported rather than wrapped. Future-dated references follow the
same policy without waiting for the clock.

The nonce is generated before signing, so only the final candidate is signed.
Its marker identifies it as ngit-owned; a previous ngit nonce is replaced on a
later update, while unrelated nonce tags are preserved.

### Strict timestamp ordering

Proposal histories always use a timestamp strictly after their reference when
the wall clock has not already advanced. Their readers use the newest timestamp
to select the active revision before walking its thread, so an event-ID tie is
not sufficient even though it is deterministic.

Every event in one patch revision, including its optional cover letter, receives
the same explicit timestamp. This keeps the revision coherent if signing spans
a wall-clock second. A rapid later revision receives the next timestamp, so it
cannot be confused with the revision it supersedes.

PR updates and patch-to-PR upgrades are also timestamped strictly after the
latest event in their proposal history. The addition is checked, so timestamp
overflow is reported rather than wrapped. Future-dated references are advanced
without waiting for the clock.

## Guarantees

Each affected event sorts after its reference according to the policy its
consumers implement: NIP-01 replacements, including repository state, have
either a later timestamp or the same timestamp with a lower ID, while
proposal-history updates have a strictly later timestamp. Planned state events
are cached before relay publication so an immediate remote-helper process can
order against its predecessor without waiting for relay propagation. This keeps
relay replacement, GRASP authorization, and proposal-tip selection consistent
without test or production sleeps.
