# Event `created_at` Ordering

Status: implemented.

## Problem

Nostr timestamps have one-second resolution. If ngit publishes a replacement
event in the same second as the event it supersedes, relays and consumers use
the event ID as a tiebreaker. Under NIP-01, the lower ID wins. A normally
created replacement therefore has an unpredictable chance of losing to the
older event.

Proposal history has a related interoperability constraint. Readers select the
latest revision using timestamp and the lower-ID tie-break, then follow its
thread. Writers deliberately advance revision timestamps so clients that omit
the tie-break still see the new revision.

The same problem occurs when the current event is future-dated: publishing at
the wall-clock time cannot replace it until the clock catches up.

## Scope

ngit applies explicit ordering to repository and proposal metadata:

- repository state events (kind 30618),
- repository announcements (kind 30617),
- patch revision series (kind 1617),
- pull-request upgrades and updates (kinds 1618 and 1619), and
- proposal statuses (kinds 1630 through 1633),
- subject and cover-note overrides (kinds 1985 and 1624),
- public GRASP and private Git relay lists (kinds 10317 and 10318), and
- software applications, releases, container repositories, and nsite manifests.

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

Tag-bearing single-event finalization uses `finalize_ordered_unsigned` with a required
`OrderingPolicy`; there is no implicit default. The policy owns the timestamp;
a custom timestamp left on a reused builder cannot override it:

| Policy | Callers | Same-second or future predecessor |
| --- | --- | --- |
| `PreferSameTimestamp` | State, announcements, statuses, containers, software applications | Bounded lower-ID search, then checked timestamp advancement |
| `StrictlyLater` | PR upgrades/updates, subject/cover-note edits, public GRASP lists, changed nsite manifests | Checked timestamp advancement, followed by the ID guard |
| `PreserveTimestamp(date)` | Initial releases and release edits | Bounded lower-ID search at the explicit date; exhaustion is an error |

Patch series use the shared `strictly_later_timestamp` calculation once to keep
every event in one revision on the same timestamp. It implements the same
strict advancement rule as `StrictlyLater`. Tagless private Git relay lists
also use this calculation directly, preserving their empty public tag list.

Release dates are domain metadata as well as event timestamps. An explicit
older date is rejected; an explicitly newer date only needs the ID guard. Fixed-date
exhaustion must not silently change the release date.

Timestamp advancement never waits for the wall clock. Rapid updates and
future-dated predecessors may produce future timestamps; checked overflow is an
error. Unchanged nsites reuse the existing event without ordering or signing.

Readers must still use the canonical latest timestamp and lower-ID tie-break.
Strict revision timestamps also protect interoperability with clients that
do not resolve timestamp ties correctly.


### Avoiding difficult predecessor IDs

Fresh-timestamp events avoid the lowest 5% of IDs when there is no predecessor
or it is more than five seconds old (about 1.05 hashes on average). If the
predecessor is within five seconds, including exactly five seconds or a
future-dated predecessor, they avoid the lowest 50% (about two hashes).
Recency uses the observed clock, not an advanced or semantic release timestamp.
If the bounded search misses that preference, it keeps the highest acceptable
ID found. The hard minimum leading 64-bit prefix remains `2F`, where
`F = ceil(2^64 / 10_000)`, leaving room for a replacement within the
expected-attempt budget. The ID is computed before signing, and only the
final event is signed. Private Git relay lists (kind 10318) are tagless by
contract: they use the shared strict timestamp calculation without an ID
preference or nonce. Their next replacement always advances the timestamp.

At an equal timestamp, ngit prefers the highest 5% of winning IDs when that
is cheap. For leading predecessor prefix `P`, the preferred minimum is
`P - max(P / 20, F)`. The acceptable range widens as inherited difficulty
grows, keeping the expected search cost within 10,000 attempts. At the
feasibility limit, any winner qualifies.

This is a preference: if the bounded search finds only harder winners, it
returns the highest-ID winner found. Future replacement difficulty never
disqualifies an otherwise valid winner. Feasibility independently bounds the
raw cost of finding any lower ID at 10,000 expected attempts. An inherited
predecessor below the fresh-event floor can therefore still be replaced.
If no winner is found or the predecessor is already too expensive, the selected
policy advances the timestamp or returns a fixed-date error. Arbitrarily many
edits at one semantic date are not guaranteed.

All searches are bounded at 100,000 nonce attempts. A fresh-timestamp safety
search that exhausts its limit returns an error instead of publishing an unsafe
ID. Strict timestamp ordering may therefore add a nonce, but never mines to
beat the previous ID. Patch series keep their single strict timestamp; their
non-replaceable per-commit events do not need this guard.

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
the wall clock has not already advanced. This protects clients that do not
correctly implement the lower-ID tie-break; readers still must implement it.

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

## Writer and fixture audit

Subject and cover-note edits use the same authorized winner selection as their
readers, including edits by other repository members. Unauthorized events do
not influence the replacement timestamp. Public GRASP lists retain their source
event and use the lower-ID tie-break when loading it, just like other replaceable
metadata. Fresh-account profiles and relay lists have no predecessor but use the shared
finalizer to guard their initial IDs. Automatic issue
resolution and patch-to-PR close statuses also use the shared status policy,
including NIP-34 status references without a NIP-10 root marker.

The harness compiles the pure production ordering module directly, avoiding a
circular dependency on ngit. `finalize_ordered_fixture` requires a policy and is
used for normal fabricated replacements: repository state, public GRASP lists,
relay lists, and announcement changes. Explicit historical timestamps, invalid
payloads, equal-ID-ordering candidates, and fixed release dates remain raw
fixtures because forcing them to win would invalidate the test. Production-driven
PR and patch fixtures run the real writers. A patch series calculates its shared
strict timestamp once; each patch must retain that timestamp when signed.

Ordering is relative to the observed predecessor, not a distributed lock:
concurrent writers with stale caches can still race. Normal fetching and cache
updates remain necessary.
