# Maintainer Model

How ngit handles multi-maintainer repositories: coordinate discovery, maintainer sets, and the distinction between shared metadata and personal infrastructure.

## Coordinate Discovery

A **coordinate** is a `(kind, pubkey, identifier)` tuple that uniquely identifies a repository on nostr. The pubkey in the coordinate is the **trusted maintainer** (typically the original creator).

ngit discovers the coordinate locally from (in priority order):

1. `nostr://` git remotes
2. `nostr.repo` git config
3. `maintainers.yaml`

No network access is required to find the coordinate. The coordinate may exist without a corresponding announcement event on relays.

## Maintainer Set

Each repository announcement (kind 30617) contains a `maintainers` tag listing public keys. These form a recursive set: if Alice lists Bob, and Bob lists Carol, then {Alice, Bob, Carol} are all in the maintainer set.

Each maintainer independently decides who they list. Adding someone to your maintainers tag is an invitation to co-maintain.

## Consuming vs Publishing

The key architectural distinction is between **consuming** repository data (fetching, cloning, listing) and **publishing** it (`ngit init`).

### Consuming: Union Across Maintainers

When consuming repo data, `relays`, `clone` (git server URLs), and `blossoms` are **unioned** across all maintainers' announcement events. This means any maintainer can add a mirror git server or relay and all users benefit automatically.

### Publishing: Personal Infrastructure, Shared Metadata

When publishing via `ngit init`, fields are sourced differently depending on their type:

#### Shared Metadata

Sourced from the **latest event** (by `created_at`) across the maintainer set:

- `name`
- `description`
- `web`
- `hashtags`

Rationale: these are shared identity. If any maintainer updates the project name, all subsequent re-announcements should pick it up.

#### Infrastructure (Personal)

Each maintainer has their own infrastructure preferences. When publishing, infrastructure comes from **my own announcement only**, not the union:

- **Grasp servers** -- where my git+nostr data is hosted. Each grasp server derives:
  - Clone URL: `https://{server}/{npub}/{identifier}.git`
  - Relay URL: `wss://{server}`
  - Blossom URL: `https://{server}`
- **Additional relays, git servers, blossoms** -- beyond what grasp servers provide

Grasp-format clone URLs belonging to other maintainers are kept as additional git servers (they're part of the union for consumers) but are not treated as my grasp servers.

#### Maintainers

Sourced from **my own announcement only**. Each maintainer independently decides who they list.

If I don't have an existing announcement (first time co-maintaining), the default is `[me, trusted_maintainer]`.

#### Earliest Unique Commit

Cascade: my own event's value, then other maintainers' values, then the local root commit. A mismatch between maintainers may indicate a fork.

#### Identifier

From the existing coordinate. Cannot change without `--force` (changing it creates a new repository).

## Init States

When `ngit init` runs, there are 5 possible states based on what exists locally and on relays:

| State | Condition | Behavior |
|-------|-----------|----------|
| **Fresh** | No coordinate found | Must provide name + server infrastructure |
| **Coordinate Only** | Coordinate exists, no announcement on relays | Requires `--force` (could be a relay/network issue) |
| **My Announcement** | Announcement exists, I'm the trusted maintainer | Re-publish/update, no force needed |
| **Co-Maintainer** | Announcement exists, I'm listed as maintainer | Publish own announcement, no force needed |
| **Not Listed** | Announcement exists, I'm not in maintainer set | Requires `--force` |

See `src/bin/ngit/sub_commands/init.rs` (`InitState` enum) and `tests/ngit_init.rs` for the implementation and test coverage.

## Why Each Co-Maintainer Must Publish Their Own Announcement

### The Scam Scenario

Nostr git repository state is tracked by Kind:30618 events (the "git state" event).
These events say: "for the repository with identifier `X`, the current branch tips are...".

Crucially, a state event knows only its identifier (`d` tag) — not which specific
coordinate chain (which trusted maintainer's pubkey) it belongs to.

This creates an attack vector:

1. Alice has a reputation in the Rust ecosystem. She contributes to `my-lib` and
   is listed in the trusted maintainer's Kind:30617 announcement.
2. Alice pushes and, in doing so, publishes a Kind:30618 state event for
   identifier `my-lib`.
3. A scammer creates a completely different, malicious repository — also with
   identifier `my-lib` — and publishes their own Kind:30617 listing Alice as a
   maintainer.
4. The scammer points to Alice's state event as "proof" that Alice maintains their
   project. Clients that filter state events by maintainer pubkey would include
   Alice's state event when fetching the scam repo.
5. Alice's reputation is attached to a project she has never heard of.

### Why the Announcement Resolves This

A Kind:30617 announcement is a signed statement from Alice that says:

> "I, Alice (pubkey X), am a maintainer of the repository at identifier `my-lib`
> whose trusted maintainer is pubkey Y."

The coordinate used to discover Alice's announcement is
`30617:Y:{identifier}` — it is rooted at the trusted maintainer's pubkey, not the
identifier alone. Alice's own announcement event, signed by Alice's key, is published
under that same coordinate chain (because `get_repo_ref_from_cache` walks the
maintainer graph starting from Y's event, and finds Alice's event because Alice
listed herself as a maintainer of the same identifier).

A scammer's fake `my-lib` has a different trusted maintainer pubkey (Z, not Y). Even
if Z lists Alice in their maintainers tag, Alice's existing Kind:30617 under the
`30617:Y:my-lib` coordinate chain does NOT appear under `30617:Z:my-lib`. The scammer
cannot bootstrap from Alice's existing announcement.

And crucially: Alice's Kind:30618 state events are only fetched when a client is
looking at a coordinate chain that Alice has explicitly announced. If Alice has never
published a Kind:30617 for `30617:Z:my-lib`, her state events are not fetched in
that context.

Without Alice's announcement, her state events carry no coordinate chain membership.
With her announcement, her state events are trusted only within the chains she has
explicitly joined.

### The Remaining Vulnerability Without Announcements

If we allowed state events without announcements — i.e., we filtered state events
by `repo_ref.maintainers` even for maintainers without their own Kind:30617 — the
attack above works:

- The scammer publishes `30617:Z:my-lib` listing Alice.
- `get_repo_ref_from_cache` for `Z`'s coordinate now includes Alice in `maintainers`.
- `get_filter_state_events` includes Alice's pubkey in the author filter.
- Alice's legitimate state events for the real `my-lib` are fetched and used by the
  scam repo.

The scammer cannot forge Alice's state events (they're signed), but they can attribute
real ones to their fake project. A user fetching the scam repo sees a real commit
history, ostensibly co-maintained by Alice, with no indication anything is wrong.

### Asymmetric Enforcement: Push vs Fetch

ngit enforces the announcement requirement on push only. When fetching, state events
are accepted from any pubkey in the maintainer set, regardless of whether that pubkey
has published its own Kind:30617. This encourages good practice while remaining
resilient when other tools don't follow the same pattern.

- **Push (strict)**: ngit will not publish a state event for a co-maintainer who
  lacks an announcement. If the user has no announcement, ngit auto-publishes one
  with defaults before proceeding. This ensures that every state event ngit produces
  is backed by an explicit, signed opt-in.

- **Fetch (permissive)**: state events from announcement-less maintainers are still
  accepted. This keeps ngit interoperable with other tools that may not enforce the
  announcement requirement, and avoids silently dropping legitimate state from
  maintainers who used a different client.

The scam scenario is therefore partially mitigated rather than fully prevented: a
scammer can still attribute Alice's state events to a fake coordinate chain if Alice
has never pushed via ngit. The push-side requirement limits the window of exposure
to maintainers who have only ever used non-compliant tooling.
