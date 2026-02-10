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
