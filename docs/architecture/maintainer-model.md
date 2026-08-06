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

Each maintainer independently decides who they list. A directional listing immediately places that pubkey in the recursive maintainer set: their repository events are authoritative. Calling the relationship an **invitation** describes its unreciprocated social state, not reduced permissions.

## Maintainer Roles

- **Selected maintainer**: the pubkey in the coordinate selected by the user's
  `nostr://` URL or config. This maintainer's announcement is the anchor for
  repository discovery and should be listed first in repository `a` tags on
  proposals.
- **Lead maintainer**: an optional coordination role inferred when exactly one
  confirmed maintainer is listed by strictly more confirmed maintainers than every other
  maintainer. If the count ties, UIs should omit the lead indication rather than
  assert that no lead exists. If one maintainer is intended to be the lead,
  co-maintainers may list only that lead in their own announcement, letting the
  lead remove co-maintainers unilaterally by changing the lead's maintainer
  list. This inferred lead is informational, distinct from the selected
  maintainer. It grants no additional permission. ngit displays it when unique.
- **Confirmed co-maintainer**: a maintainer in the reciprocally connected group
  containing the selected maintainer. Confirmation is derived from graph edges,
  not merely from whether a same-identifier announcement exists.
- **Invited maintainer**: an authorized maintainer reached through a directional
  edge who has not reciprocally connected to the selected group. The familiar
  invitation framing prevents an unsolicited listing from implying endorsement;
  it does not withhold maintainer rights.

The selected maintainer is not necessarily the lead maintainer. Different users
can select different maintainers for the same repository by using different
`nostr://` URLs, while still discovering the same recursive maintainer graph.

## Announcement Tag Ordering

Proposal events such as patches and pull requests tag repository announcements with `a` tags. These tags should be ordered as:

1. the selected maintainer's announcement coordinate,
2. other confirmed maintainers' announcement coordinates,
3. invited maintainers' coordinates.

Invited maintainers are included because they are part of the authorized directional graph. They come last for compatibility because their announcement may not exist yet. Putting the selected maintainer first gives clients a stable trust and discovery anchor; it does not grant that maintainer greater authority.

## Consuming vs Publishing

The key architectural distinction is between **consuming** repository data (fetching, cloning, listing) and **publishing** it (`ngit init`).

### Consuming: Union Across Maintainers

When consuming repo data, `relays`, `clone` (git server URLs), and `blossoms` are **unioned** across all maintainers' announcement events. This means any maintainer can add a mirror git server or relay and all users benefit automatically.

Clone URLs may use installed Git remote helpers. Installing a
`git-remote-<scheme>` executable is treated as consent for clone URLs in signed
maintainer announcements to invoke it, subject to Git's
`protocol.<scheme>.allow` policy. Because infrastructure is unioned, this trust
applies to helper URLs published by any discovered maintainer. ngit blocks
recursive `nostr` URLs and Git's internal `fd` transport, while `ws` and `wss`
remain reserved for GRASP bases.

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

If I don't have an existing announcement, non-interactive acceptance lists me plus the sole confirmed maintainer or unique inferred lead. If leadership is ambiguous, ngit retains the selected maintainer as the compatibility default.

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
| **Invited Maintainer**     | I'm directionally listed but not reciprocally connected                    | Already authorized; publish my relationship choice, no force needed |
| **Confirmed Co-Maintainer**| I'm in the selected maintainer's reciprocal group                          | Re-publish/update my announcement, no force needed  |
| **Not Listed**             | Announcement exists, I'm not in maintainer set                             | Requires `--force`                                  |

See `src/bin/ngit/sub_commands/init.rs` (`InitState` enum) and the `tests/init_state_*` integration tests for the implementation and test coverage.

## Why Reciprocal Announcements Matter

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

### Why Reciprocity Resolves the Display Problem

A Kind:30617 announcement is an author-keyed statement. Alice's announcement
has coordinate `30617:Alice:{identifier}` and describes the maintainers Alice
recognizes for her repository with that identifier. It is never published under
the selected maintainer's coordinate.

If the selected group lists Alice, clients immediately include Alice in the
directional maintainer graph and accept her repository events as authoritative.
If Alice's own announcement also creates a path back to that group, the
relationship is reciprocal and clients may present Alice as a confirmed
co-maintainer.

A scammer can still list Alice and thereby authorize Alice's real
same-identifier events in the scammer's directional graph. The safety boundary
is therefore honest presentation: until Alice reciprocates, clients show her as
invited rather than implying that she endorsed the association. Invitation is a
relationship state, not an authorization restriction.

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

### Push and Fetch

ngit publishes an announcement before its own push path emits state for a
maintainer without one. This records the user's relationship choice; it does not
activate rights that were previously absent.

When fetching, state events are accepted from every pubkey in the directional
maintainer set, regardless of reciprocity or whether that pubkey has published
their own Kind:30617. The scam scenario is therefore not prevented at the
authorization layer. It is made clear at the presentation layer by separating
authorized invitations from reciprocal, confirmed membership.
