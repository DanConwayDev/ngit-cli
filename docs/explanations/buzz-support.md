# Buzz Repository Support

Status: transport support implemented; collaboration interoperability remains
partial.

## Compatibility target

This document records the compatibility boundary intended for the next minor
ngit release. It is an evaluation of specific versions, not a promise about all
future Buzz releases.

| Component | Evaluated version | Revision |
| --- | --- | --- |
| ngit | v3 (planned) | `6dc200aa56a15d84f67f58621cce69f9b9603c6d` |
| Buzz Desktop | 0.5.11 | `b12739b23da92b0f1e99626b02749ab55c51b8ce` |
| `buzz-relay` | 0.2.1 | `b12739b23da92b0f1e99626b02749ab55c51b8ce` |

The ngit integration fixture pins that exact Buzz revision in `flake.nix`.
The Buzz Desktop and relay versions are recorded separately because the relay
has an independent release cadence, while the repository UI and its Nostr
event conventions ship with Buzz Desktop.

## Authority models

Buzz and ngit have different definitions of repository authority. Most of the
remaining compatibility work follows from this difference.

In ngit, the author of a kind 30617 repository announcement is the initial
maintainer. Additional maintainers are discovered through announcement
relationships. Repository state events are normally trusted only when their
signer is in that maintainer graph.

In Buzz, a repository is owned by the kind 30617 author but hosted and
authorized by a Buzz server. The `buzz-channel` tag binds it to a channel ACL.
Channel members may have Git permissions without appearing in the NIP-34
maintainer graph.

Buzz stores the Git repository as a server-side manifest. Its Smart HTTP
advertisement is derived from that manifest. After committing a push, Buzz
publishes a kind 30618 event derived from the committed manifest and signs it
with the relay key, not the repository owner's or pusher's key. Buzz Desktop
therefore trusts repository state signed by either the repository owner or the
relay itself.

For a Buzz-hosted repository, the intended source of truth is consequently:

- the authenticated Buzz Git server for `HEAD`, branches, tags, and objects;
- the repository owner for the kind 30617 identity and metadata;
- the Buzz channel ACL for read and write permission; and
- Nostr proposal events for pull-request metadata and lifecycle state.

## Implemented transport support

ngit recognizes a preserved foreign `buzz-channel` tag as a private repository
marker. This enables the private-repository policy without stripping the tag
if ngit republishes the announcement.

The implemented transport path provides:

- repository-relay-only discovery and event routing;
- NIP-11-assisted classification of copied Buzz repository URLs;
- NIP-42 authentication to the Buzz relay;
- repository-scoped NIP-98 authentication for Git Smart HTTP;
- authenticated clone, fetch, and push for an authorized signer;
- rejection of unauthenticated users and non-channel members; and
- suppression of private repository announcements and state on public or
  account relays.

These are transport guarantees. They do not imply that ngit and Buzz interpret
all repository, permission, or pull-request events identically.

## Normal branches and tags

ngit always queries the declared Git servers when listing a remote. It also
fetches repository state events from the repository relay.

For normal refs, ngit currently selects state as follows:

1. Consider the newest kind 30618 captured from each relay.
2. Discard candidates not signed by a repository maintainer.
3. Prefer the newest remaining event whose object IDs are present on a Git
   server or already available locally.
4. Fall back to the first declared Git server's advertised refs when there is
   no acceptable event.

A Buzz relay-signed 30618 is not a maintainer-signed candidate. When it is the
newest event observed on the relay, ngit commonly falls back to the live Buzz
Git advertisement and obtains the correct refs. This is fallback behavior,
not a guarantee that the server is authoritative. A resolvable maintainer
event can still override a newer Git server tip; an old commit remains
resolvable after its branch advances.

Some cache consumers are stricter. Push planning and explicit state lookups
read maintainer-signed state only. They do not use the relay-signed Buzz state
as an authoritative snapshot. An owner can therefore construct a new state
event from stale cached state after another Buzz member changed a different
branch or tag. The resulting owner-signed event may omit or rewind refs that
still exist correctly on the Buzz server.

The compatibility policy should make the authenticated Buzz Git advertisement
authoritative for normal refs. A future implementation can instead trust the
relay-signed 30618 only if the relay key is securely bound to the declared Buzz
server. Directly using the Git advertisement is simpler and naturally handles
relay-key rotation.

## Channel members and maintainers

Buzz permits ordinary channel members to create branches and tags and to
fast-forward branches by default. Force pushes, deletions, and moving tags
normally require a stronger channel role.

ngit currently rejects a normal branch or tag push before contacting the Git
server when the active signer is not in the repository's NIP-34 maintainer
graph. Consequently, an ordinary Buzz channel member may be allowed to push by
Buzz but unable to perform that same push through a `nostr://` remote.

For Buzz repositories, ngit should let the authenticated Buzz server make the
normal-ref authorization decision. ngit must not require a server-authorized
channel member to publish a maintainer-signed 30618; Buzz already publishes the
post-commit relay-signed state event.

## Pull requests

### Metadata and lifecycle

The core pull-request event flow is compatible:

| Capability | Buzz event | ngit support |
| --- | --- | --- |
| Pull-request root | kind 1618 | Yes |
| Updated tip | kind 1619 with `c`, `clone`, and root `E` tags | Yes |
| Open, merged, closed, and draft state | kinds 1630-1633 | Yes |
| Source branch name | `branch-name` | Yes |
| Tip commit | `c` | Yes |
| Clone locations | `clone` | Yes |

Online ngit PR commands refresh repository events before reading the local
cache. The Git remote helper also refreshes Nostr state before advertising
refs. A Buzz PR root, its later 1619 updates, and its lifecycle status should
therefore remain current while the repository relay is reachable.

### Git object availability

Routine clone and fetch operations only fetch PR object IDs from the
repository's declared Git servers. They deliberately do not contact arbitrary
`clone` URLs supplied by PR authors during a passive operation.

This works for the usual Buzz flow because the source branch is already hosted
and advertised by the same Buzz repository. ngit can obtain the object and
synthesize a `pr/*` ref at the latest `c` tag.

An explicit `ngit pr checkout` is allowed to try clone URLs from the selected
PR event before falling back to repository servers. It prepares private Git
authentication for each URL. This supports an intentional checkout from a
separate source server without giving every PR author an automatic network
request during ordinary clone or fetch.

A commit-only PR event is not an object archive. A forked PR can have visible
metadata but no usable Git data during routine fetch if its tip is absent from
the canonical Buzz server. The same applies when its source branch is deleted
before the object is downloaded. Buzz should retain PR tips under durable
server refs, or ngit should allow routine fetching when a PR clone URL is
verified to identify the already-trusted Buzz host.

An existing local PR branch follows normal Git safety rules. ngit can
fast-forward it to a newer 1619 tip and preserves local commits on top. A
force-pushed proposal that diverges from the local branch requires an explicit
`ngit pr checkout --force`.

### Conversation and reviews

Conversation interoperability is not implemented.

Buzz publishes ordinary comments, inline comments, review requests,
approvals, and change requests as kind 1 text notes. They use a lowercase `e`
tag for the PR root and `t` labels for review semantics. Buzz does this because
the evaluated relay does not register NIP-22 kind 1111.

ngit fetches and displays PR comments as kind 1111 events with an uppercase
`E` root tag. It does not fetch Buzz's kind 1 PR comments. Conversely, a kind
1111 comment published by ngit is not accepted by the evaluated Buzz relay.

As a result, ngit sees the PR description, tip updates, and lifecycle status,
but not the Buzz discussion or review timeline. Compatibility requires either:

- Buzz accepting kind 1111 and dual-reading existing kind 1 history; or
- a Buzz adapter in ngit that reads and publishes the legacy kind 1 shape.

Review labels such as `review-request`, `approval`, `changes-requested`, and
`inline-comment` also need explicit interpretation before ngit can represent
Buzz review state rather than merely render those events as comments.

### Target branches

Buzz writes the destination as `target-branch`. ngit writes and reads the
destination as `b`. Neither client currently reads the other's tag.

This is usually invisible for a PR targeting the repository default branch,
because both clients fall back to that branch. It is incorrect for release,
maintenance, or stacked PRs targeting any non-default branch. During a
transition, both clients should read both spellings and emit both tags.

## Integration-test boundary

The existing `private_buzz` integration test verifies:

- private URL discovery and authentication;
- an owner push through the ngit remote helper;
- authenticated clone and a second owner-signed push;
- denial of unauthenticated and outsider access;
- preservation of the `buzz-channel` and `clone` announcement tags;
- publication of an owner-signed repository state event; and
- absence of private announcement and state events on public relays.

The variable named `member` in that test still authenticates as the repository
owner. The test does not create a distinct channel member, and its state query
is restricted to the owner's pubkey. It therefore does not exercise Buzz's
relay-signed 30618 authority or the difference between channel membership and
NIP-34 maintainership.

It also does not exercise PR creation, revisions, Git object retrieval,
comments, reviews, non-default targets, tags, branch deletion, or a Buzz-native
push followed by an ngit operation.

## Release acceptance tests

Before describing ngit v3 as having full Buzz repository support, test at
least these workflows with the pinned compatibility target:

1. Create separate owner, ordinary-member, admin, and outsider identities.
   Verify clone and normal-ref permissions for each role through `nostr://`.
2. Push a branch and tag directly through Buzz as an ordinary member. Fetch
   them through ngit, then have the owner push an unrelated ref. Verify that a
   fresh ngit clone still advertises every live server ref.
3. Create a PR in Buzz, then add an ordinary comment, inline comment, review
   request, approval, change request, update, draft transition, close, reopen,
   and merge. Compare the complete timeline in Buzz and ngit.
4. Create equivalent PRs from ngit and Buzz targeting a non-default release
   branch. Verify display, checkout, and merge-base selection in both clients.
5. Update and force-push a PR source branch. Verify normal fetch, repeated
   checkout, divergence handling, and the latest synthesized `pr/*` ref.
6. Exercise same-repository and fork clone URLs, then delete the source branch.
   Record when the PR commit remains retrievable.
7. Repeat discovery and collaboration operations while observing all public
   relays. No private repository coordinate or event may fan out beyond the
   Buzz repository relay.

Until those cases pass, the v3 supported claim should remain **basic Buzz
support**: cloning Buzz repositories and viewing, creating, updating, and
changing the lifecycle status of pull requests. Normal branch and tag pushes
and pull-request comments are excluded.
