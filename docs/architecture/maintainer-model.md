# Maintainer Model

How ngit handles multi-maintainer repositories: coordinate discovery, maintainer roles, and the distinction between shared metadata and personal infrastructure.

## Coordinate Discovery

A **coordinate** is a `(kind, pubkey, identifier)` tuple that uniquely identifies a repository on nostr. The pubkey in the coordinate is the **selected maintainer**: the maintainer chosen by the `nostr://` URL, `nostr.repo` config, or explicit coordinate the user is consuming.

ngit discovers the coordinate locally from (in priority order):

1. `nostr://` git remotes
2. `nostr.repo` git config
3. `maintainers.yaml`

No network access is required to find the coordinate. The coordinate may exist without a corresponding announcement event on relays.

## Maintainer Set

Each repository announcement (kind 30617) contains a `maintainers` tag listing public keys. These form a recursive graph: if Alice lists Bob, and Bob lists Carol, then {Alice, Bob, Carol} can all be discovered from Alice's selected coordinate.

Each maintainer independently decides who they list. Adding someone to your maintainers tag is an invitation to co-maintain.

## Maintainer Roles

- **Selected maintainer**: the pubkey in the coordinate selected by the user's
  `nostr://` URL or config. This maintainer's announcement is the anchor for
  repository discovery and should be listed first in repository `a` tags on
  proposals.
- **Lead maintainer**: an optional UI-level role inferred when exactly one
  maintainer is listed by strictly more recursive maintainers than every other
  maintainer. If the count ties, UIs should omit the lead indication rather than
  assert that no lead exists. If one maintainer is intended to be the lead,
  co-maintainers may list only that lead in their own announcement, letting the
  lead remove co-maintainers unilaterally by changing the lead's maintainer
  list. This inferred lead is informational, distinct from the selected
  maintainer, and not displayed by ngit's CLI.
- **Co-maintainer**: a discovered maintainer who has published their own kind
  30617 announcement for this repository identifier. Co-maintainers are shown in
  `ngit repo`.
- **Invited maintainer**: a pubkey listed by a co-maintainer, but with no
  discovered kind 30617 announcement of its own. Invited maintainers are still
  tagged for discovery, but are not shown as co-maintainers until they publish an
  announcement.

The selected maintainer is not necessarily the lead maintainer. Different users
can select different maintainers for the same repository by using different
`nostr://` URLs, while still discovering the same recursive maintainer graph.

## Announcement Tag Ordering

Proposal events such as patches and pull requests tag repository announcements with `a` tags. These tags should be ordered as:

1. the selected maintainer's announcement coordinate,
2. other co-maintainers' announcement coordinates,
3. invited maintainers' announcement coordinates.

Invited maintainers are included so clients can discover the invitation, but they come last because their announcement may not exist yet. Putting the selected maintainer first gives clients a stable, existing announcement to anchor rendering and avoids treating an invitation as the primary repository reference.

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
- unknown/foreign tags preserved for forward compatibility (`extra_tags`), unless
  `ngit init --clean` is used

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

If I don't have an existing announcement (first time accepting an invitation), the default is `[me, selected_maintainer]`.

#### Earliest Unique Commit

Cascade: my own event's value, then other maintainers' values, then the local root commit. A mismatch between maintainers may indicate a fork.

#### Identifier

From the existing coordinate. Cannot change without `--force` (changing it creates a new repository).

## Init States

When `ngit init` runs, there are 6 possible states based on what exists locally and on relays:

| State                      | Condition                                                                  | Behavior                                            |
| -------------------------- | -------------------------------------------------------------------------- | --------------------------------------------------- |
| **Fresh**                  | No coordinate found                                                        | Must provide name + server infrastructure           |
| **Coordinate Only**        | Coordinate exists, no announcement on relays                               | Requires `--force` (could be a relay/network issue) |
| **My Announcement**        | Announcement exists, I'm the selected maintainer                           | Re-publish/update, no force needed                  |
| **Invited Maintainer**     | Announcement exists, I'm listed as maintainer but have no announcement yet | Publish own announcement to accept, no force needed |
| **Co-Maintainer**          | Announcement exists, I'm listed and have my own announcement               | Re-publish/update my announcement, no force needed  |
| **Not Listed**             | Announcement exists, I'm not in maintainer set                             | Requires `--force`                                  |

See `src/bin/ngit/sub_commands/init.rs` (`InitState` enum) and the `tests/init_state_*` integration tests for the implementation and test coverage.

## Why Each Invited Maintainer Must Publish Their Own Announcement

### The Scam Scenario

Nostr git repository state is tracked by Kind:30618 events (the "git state" event).
These events say: "for the repository with identifier `X`, the current branch tips are...".

Crucially, a state event knows only its identifier (`d` tag) — not which specific
coordinate chain (which selected maintainer's pubkey) it belongs to.

This creates an attack vector:

1. Alice has a reputation in the Rust ecosystem. She contributes to `my-lib` and
   is listed in the selected maintainer's Kind:30617 announcement.
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
> whose selected maintainer is pubkey Y."

The coordinate used to discover Alice's announcement is
`30617:Y:{identifier}` — it is rooted at the selected maintainer's pubkey, not the
identifier alone. Alice's own announcement event, signed by Alice's key, is published
under that same coordinate chain (because `get_repo_ref_from_cache` walks the
maintainer graph starting from Y's event, and finds Alice's event because Alice
listed herself as a maintainer of the same identifier).

A scammer's fake `my-lib` has a different selected maintainer pubkey (Z, not Y). Even
if Z lists Alice in their maintainers tag, Alice's existing Kind:30617 under the
`30617:Y:my-lib` coordinate chain does NOT appear under `30617:Z:my-lib`. The scammer
cannot bootstrap from Alice's existing announcement.

Alice's Kind:30617 announcement gives clients a signed opt-in that binds her pubkey
to one coordinate chain. Without that announcement, Alice is only invited: her pubkey
appears in someone else's maintainer list, but she has not signed a statement joining
that repository.

This distinction lets ngit require an announcement before publishing Alice's own
state events, and lets UIs avoid displaying an invited maintainer as a
co-maintainer.

### The Remaining Vulnerability Without Announcements

If ngit allowed publishing state events without announcements — i.e., if it let an
invited maintainer push without first publishing their own Kind:30617 — the attack
above would be easier to exploit:

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

- **Push (strict)**: ngit will not publish a state event for an invited maintainer who
  lacks an announcement. If the user has no announcement, ngit auto-publishes one
  with defaults before proceeding. This ensures that every state event ngit produces
  is backed by an explicit, signed opt-in.

- **Fetch (permissive)**: state events from invited maintainers are still
  accepted. This keeps ngit interoperable with other tools that may not enforce the
  announcement requirement, and avoids silently dropping legitimate state from
  maintainers who used a different client.

The scam scenario is therefore partially mitigated rather than fully prevented: a
scammer can still attribute Alice's state events to a fake coordinate chain if Alice
has never pushed via ngit. The push-side requirement limits the window of exposure
to maintainers who have only ever used non-compliant tooling.
