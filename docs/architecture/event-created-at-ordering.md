# Event `created_at` Ordering

Status: implemented.

## Problem

Nostr timestamps have one-second resolution. If ngit publishes a replacement
event in the same second as the event it supersedes, relays and consumers use
the event ID as a tiebreaker. Under NIP-01, the lower ID wins. A normally
created replacement therefore has an unpredictable chance of losing to the
older event.

The same problem occurs when the current event is future-dated: publishing at
the wall-clock time cannot replace it until the clock catches up.

## Scope

ngit applies explicit ordering to replaceable repository and proposal metadata:

- repository state events (kind 30618),
- repository announcements (kind 30617),
- pull-request updates (kind 1619), and
- proposal statuses (kinds 1630 through 1633).

Patch events are not included because their NIP-10 threading establishes
revision order. Initial pull-request events also retain their normal timestamp;
only updates must supersede an existing pull-request history.

## Ordering policy

Before publishing an affected event, ngit identifies the event it must replace:

- state and announcement updates use the canonical latest event for the
  repository across maintainers;
- a pull-request update considers its root event and every prior update; and
- a proposal status considers all status kinds in the same proposal thread.

In every case, the latest `created_at` wins. For equal timestamps, the lowest
event ID wins, matching NIP-01. Only events already present in the local cache
can be considered, so normal fetch-before-publish flows remain important.

If the normal current timestamp is newer than the reference event, ngit uses it
unchanged. Otherwise it tries to publish at the reference timestamp with a
lower event ID by varying an ngit-owned nonce tag. This is bounded: ngit only
attempts grinding when the reference ID makes it practical, and stops after a
finite attempt budget.

If grinding is unsuitable or does not produce a lower ID, ngit publishes at one
second after the reference timestamp. The addition is checked, so timestamp
overflow is reported rather than wrapped. Future-dated references follow the
same policy without waiting for the clock.

The nonce is generated before signing, so only the final candidate is signed.
Its marker identifies it as ngit-owned; a previous ngit nonce is replaced on a
later update, while unrelated nonce tags are preserved.

## Guarantees

Each affected replacement event sorts after its reference event by having either
a later timestamp or the same timestamp with a lower ID. This makes ordering
consistent between ngit, relays, local signers, and remote signers while
preserving the existing behavior of patches and initial pull requests.
