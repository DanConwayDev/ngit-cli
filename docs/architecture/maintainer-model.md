# Maintainer Model

> **Historical baseline:** this document describes maintainer behavior at the
> `pr/indexed-maintainer-roles` merge-base, before ngit understood indexed
> `M`, `m`, or `o` role tags. It is a comparison point for the changes that
> follow, not a proposal for future behavior.

ngit repositories are described by replaceable kind `30617` announcements.
Clients start from one selected announcement coordinate, recursively discover
every pubkey named by `maintainers` tags, and consolidate the announcements and
repository events reachable from that coordinate.

## Everyday Mental Model

- The **selected maintainer** is the pubkey in the `nostr://` URL or local
  `nostr.repo` coordinate from which discovery starts.
- A **listed maintainer** is any pubkey recursively reachable through
  `maintainers` tags. Every listed maintainer has repository authority,
  whether or not they have published an announcement.
- A **confirmed co-maintainer** belongs to the reciprocal graph containing the
  selected maintainer. Confirmation affects presentation, not authority.
- An **invited maintainer** is listed but has not established a path back to
  the selected maintainer. The invitation label is social framing: invitees
  already have the same repository authority as confirmed maintainers.
- A **lead maintainer** is inferred when one confirmed maintainer receives more
  maintainer listings than every other confirmed maintainer. Lead is an
  informational coordination label and grants no additional authority.

There are no explicit maintainer roles, moderators, role histories, or
historical authorization intervals. The selected and inferred lead maintainers
are independent. Selecting a different coordinate may produce a different
recursive graph, confirmed group, or inferred lead.

## Normal Workflows

### Create a repository

```bash
ngit init --name "My Project" --description "What it does" --defaults --json
```

The publisher emits a `maintainers` tag containing themselves, writes their
coordinate to `nostr.repo`, points `origin` at their `nostr://` URL, and returns
that URL for sharing.

A lone maintainer is not inferred as lead. Lead inference requires at least one
maintainer-to-maintainer listing edge with a unique highest vote count.

### Invite another maintainer

The publisher replaces their own non-empty maintainer listing with themselves
and the npubs passed to `--other-maintainers`:

```bash
ngit repo edit --other-maintainers <bob-npub> --json
```

Bob is immediately part of the recursive maintainer set. His state and member
management events are authoritative even before he accepts or publishes a
kind `30617` announcement. ngit nevertheless presents Bob as invited until his
own announcement makes the relationship reciprocal.

`--other-maintainers` describes the publisher's own outgoing listing. It does
not edit any other maintainer's announcement and does not preview the resulting
graph-wide authorization changes.

### Accept a maintainer invitation

```bash
ngit repo accept --json
```

The dedicated accept command is available when the user:

- is listed in the selected repository's recursive maintainer set;
- is not the selected maintainer; and
- has not already published a same-identifier repository announcement.

Acceptance publishes the invitee's own announcement. The default listing is:

1. the invitee;
2. the sole confirmed maintainer, when there is only one;
3. otherwise the unique inferred lead; or
4. the selected maintainer when the graph has no unambiguous lead.

Acceptance changes the relationship from invited to confirmed, but it does not
grant rights: the directional invitation already granted them. The push path
can automatically publish the same acceptance before the invitee's first state
event.

Dedicated acceptance leaves `nostr.repo` and existing remotes rooted at the
inviter's coordinate. The broader `ngit init` acceptance path instead finishes
like any init publication and re-roots local configuration on the signer.

An invitee who already has a same-identifier announcement cannot use
`repo accept`; they must update their announcement through `ngit repo edit` or
`ngit init`.

### Update or remove maintainers

Without `--other-maintainers`, a publisher retains the maintainers from their
own previous announcement. Passing a non-empty `--other-maintainers` list
replaces that publisher's outgoing listing, with the publisher inserted first.

There is no membership-removal preview or maintainer-specific `--force` gate.
Removing an outgoing edge does not necessarily remove a pubkey from the
recursive set: another reachable announcement may continue to list them.

### Leave a repository

There is no `ngit repo leave` command and no wire representation for ending a
role. An announcement author is automatically inserted into their own
maintainer listing even if the `maintainers` tag omits them, so publishing an
empty or self-omitting list cannot express departure.

## Maintainers on the Wire

Current membership is carried by one deprecated NIP-34-style tag:

```text
["maintainers", "<pubkey-hex>", "<pubkey-hex>", ...]
```

The parser applies these rules:

- every pubkey in the tag is a current maintainer;
- the event author is inserted when the tag omits them;
- without a usable maintainer entry, the author is the sole maintainer;
- entries carry no role letter or time boundaries.

Publication emits the effective current list as `maintainers`. Unknown tags
round-trip through `extra_tags` unless `ngit init --clean` is used. At this
baseline, `M`, `m`, and `o` are unknown tags: they may survive republishing but
have no membership or authorization meaning.

## Current Membership Resolution

Discovery begins with the selected maintainer and repeatedly follows every
outgoing `maintainers` edge until no new pubkey is found. Missing announcements
do not remove their listed pubkeys from this recursive set.

The complete recursive set is authoritative. Reciprocity produces a separate
presentation view:

1. the selected maintainer is confirmed;
2. a discovered maintainer is confirmed when their announcement graph has a
   path back to the selected maintainer;
3. every other discovered maintainer is displayed as invited.

Because every discovered pubkey is authoritative, a downstream cycle that
does not connect back to the selected maintainer remains invited but retains
maintainer rights. `confirmed_maintainers` and `invited_maintainers` do not
form an authorization boundary at this baseline.

## Lead Resolution

Lead is inferred entirely from the reciprocal listing graph:

1. consider only confirmed maintainers;
2. count each directed listing between distinct confirmed maintainers as one
   vote for the listed pubkey;
3. select the unique pubkey with the highest positive count;
4. report no lead when the highest count is zero or shared by a tie.

A listing from an invited maintainer does not vote, and an invited pubkey
cannot be inferred as lead. A lone repository has no inferred lead. The result
is informational: inferred lead and confirmed co-maintainer have identical
state, merge, and member-management authority.

The selected maintainer is not automatically a lead. Different selected
coordinates can expose different reciprocal groups and therefore different
vote results.

## Maintainer History

There is no signed role history. The latest replaceable announcement available
for an author supplies that author's complete current listing; replacing it
can remove all evidence of an earlier relationship from relays and caches.

History does not constrain event authorization. A state, status, label,
subject, or cover-note event is evaluated against the current recursive set,
not the maintainers that were listed when the event was created.

An existing `maintainers.yaml` file can record maintainer lists through Git
commit history and remains a local coordinate fallback. It is neither a signed
Nostr history nor an input to historical event authorization.

## Inferred Leadership and Roster Changes

There is no `--lead-maintainer` flag and no explicit leadership transfer.
Clients display the unique graph-vote winner when one exists.

A conventional lead-shaped graph can be formed when co-maintainers list only
the coordinating maintainer and that maintainer lists the active roster. In a
view rooted at the coordinator, removing a co-maintainer from the coordinator's
announcement removes the only discovery edge to them unless another reachable
announcement still lists them.

This is a graph convention rather than a protected operation:

- no command verifies that the inferred lead initiated the change;
- no before/after graph simulation names lost maintainers;
- `--force` is not required for roster changes;
- another selected coordinate may retain a different view; and
- there is no pending or accepted transfer state.

## Leadless Co-maintainership

Leadless operation is ordinary. A fresh single-maintainer repository, a graph
with no maintainer edges, and a tied vote all report no lead. There is no
explicit marker distinguishing intentional co-maintainership from an
incomplete or partitioned graph.

Every listed maintainer still has equal authority. Leadlessness changes only
the coordination label shown by clients.

## Moderators

There is no moderator role. Repository-derived authority is all-or-nothing:
listed maintainers may publish repository state, merge, and perform issue and
proposal management actions. A non-maintainer may only perform actions granted
to the author of the underlying issue or proposal.

## Coordinates and Local Configuration

A coordinate is `(kind, pubkey, identifier)`. Its pubkey is the selected
maintainer and recursive discovery anchor.

Local resolution priority is:

1. explicit `--repo <REMOTE|NADDR|NOSTR-URL>`;
2. `nostr.repo`;
3. the current branch's tracked `nostr://` remote;
4. `origin` when it is `nostr://`;
5. the sole remaining distinct `nostr://` coordinate;
6. `maintainers.yaml` only when no Nostr coordinate is configured.

Before publishing a repo-scoped event, ngit prints the selected naddr and the
source used to resolve it. Selection affects discovery and display but does
not grant the selected maintainer more authority than another listed pubkey.

`ngit init` publishes the signer's announcement and then writes the signer's
coordinate to `nostr.repo` and `origin`. Dedicated `repo accept` and push-path
acceptance deliberately leave local selection on the inviter so a later
removal remains observable from that root.

There is no command for following an inferred lead or adopting another
maintainer's coordinate after a handoff.

## Consuming Repository Data

The selected announcement starts recursive consolidation. All recursively
listed maintainers, including invitees, contribute:

- repository authority;
- relay URLs;
- Git clone URLs; and
- Blossom server URLs.

Shared metadata (`name`, `description`, `web`, hashtags, and upstream metadata)
comes from the newest reachable maintainer announcement. Repository privacy is
true when any reachable announcement marks the repository private.

Installed Git remote helpers may service clone URLs in signed maintainer
announcements, subject to Git's transport policy. ngit blocks recursive
`nostr` and internal `fd` helper URLs; `ws` and `wss` remain reserved for GRASP
bases.

Kind `30618` state and maintainer-authored status, label, subject, and cover
note events are accepted from every pubkey in the flat recursive maintainer
set. Confirmation and announcement existence are not required.

## Publishing Repository Data

`ngit init` resolves fields from different sources.

Shared metadata comes from the latest reachable maintainer event:

- name;
- description;
- web URLs;
- hashtags;
- upstream metadata;
- unknown tags, unless `--clean` is passed.

Personal infrastructure comes from the publisher's own announcement, command
arguments, or configured defaults:

- grasp servers;
- additional relays;
- Git servers.

Blossom servers have no dedicated init flag at this baseline and are inherited
from the latest reachable announcement when republishing.

The publisher's maintainer listing comes from their own announcement. A fresh
publisher lists themselves; a first-time co-maintainer lists themselves and
the default acceptance target. `--other-maintainers` replaces that outgoing
listing explicitly.

The identifier comes from the existing coordinate and cannot change without
`--force`. The earliest unique commit cascades from the publisher's event, the
consolidated repository, and finally the local root commit.

## Repository Output

`ngit repo` human output separates confirmed and invited maintainers. It can
display selected and inferred-lead badges and shows each known maintainer's
outgoing relationship summary. Its invitation note explicitly says invitees
already have maintainer rights.

`ngit repo --json` exposes the flat fields:

- `maintainers`;
- `selected_maintainer`;
- `confirmed_maintainers`;
- `invited_maintainers`;
- `lead_maintainer`; and
- `maintainer_edges`.

There are no `moderators`, `confirmed_moderators`, or structured `members`
entries, and output has no role source or historical intervals.

## Announcement Tag Ordering

Proposal announcement tags are ordered:

1. selected maintainer;
2. other confirmed maintainers;
3. invited maintainers.

Invitees are included because they are already authoritative. Putting the
selected maintainer first provides a stable discovery anchor and does not grant
greater authority.

## Authorization Summary

| Actor | Repository state | Merge | Status/labels/subject/cover note |
| --- | --- | --- | --- |
| Inferred lead | yes | yes | yes |
| Confirmed co-maintainer | yes | yes | yes |
| Invited maintainer | yes | yes | yes |
| Outsider | no | no | no |

Issue and proposal authors retain the author-specific actions granted by their
event type. This table covers authority derived from repository membership.

## Baseline Edge-case Behavior

### Directional invitations grant authority

Alice listing Bob is sufficient for Bob's same-identifier repository events to
be accepted. Bob's announcement is required to display him as confirmed, not
to activate his rights. Directly published Bob events can therefore be
authoritative even if Bob never runs `repo accept`.

### Same identifier, unrelated repositories

A scammer can publish an announcement for an unrelated repository with the
same identifier and list a reputable maintainer. The reputable maintainer's
real same-identifier state or member-management events then fall inside the
scammer's recursive authorization set despite the maintainer never
acknowledging that repository. The UI labels the maintainer invited, but the
authorization layer does not prevent the attribution.

### Removal depends on every reachable edge

Omitting a maintainer from one replacement announcement does not remove them
while another reachable maintainer still lists them. Conversely, removing the
last discovery edge can discard the member and every downstream announcement,
server, relay, and metadata contribution reachable only through them. No
diagnostic previews those secondary effects.

### Existing but non-reciprocal invitee announcement

`repo accept` refuses when the invitee already has a same-identifier
announcement, even if it does not connect back to the selected group. The user
must republish through the general edit/init flow to alter their relationship.

### Replaceable announcements erase relationship history

A newer announcement supersedes its author's older listing. Relays do not
retain a canonical repository-wide history, and another maintainer's event is
not a redundant copy of the replaced author's old relationships.

### Two repositories with different histories

ngit does not merge maintainership histories or choose a canonical historical
view. It resolves current recursive membership from whichever coordinate the
checkout selected.

## Baseline Implementation Invariants

The merge-base implementation and tests establish these behaviors:

1. Kind `30617` membership is read and written through `maintainers` only.
2. An announcement author is always a member of their own listing.
3. Every recursively discovered maintainer is authoritative, including an
   invitee with no announcement.
4. Reciprocity divides confirmed from invited members for presentation only.
5. A unique positive in-degree winner among confirmed maintainers is displayed
   as lead; ties, zero-edge graphs, and lone repositories have no lead.
6. Lead, co-maintainer, and invitee have identical repository-derived rights.
7. Dedicated acceptance publishes a reciprocal announcement but does not
   change local coordinate selection.
8. Push may automatically publish acceptance before the invitee's state event.
9. There is no moderator role, leave command, explicit lead designation, or
   role-history syntax.
10. Maintainer removal has no exact authorization-loss preview or force gate.
11. Shared metadata follows the newest reachable announcement while personal
    infrastructure originates with each publisher and is unioned for readers.
12. Authorization is current-state-only and is not bounded by historical
    maintainership intervals.
