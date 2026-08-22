# Maintainer Model

This document describes the maintainer behavior implemented on the
`pr/indexed-maintainer-roles` branch. It records current behavior rather than a
future protocol or CLI design.

ngit repositories are described by replaceable kind `30617` announcements.
Clients start from one selected announcement coordinate, recursively discover
other listed pubkeys, and consolidate the announcements and repository events
that are reachable from it.

## Everyday Mental Model

- The **selected maintainer** is the pubkey in the `nostr://` URL or local
  `nostr.repo` coordinate from which discovery starts.
- A **lead maintainer** is the unique pubkey assigned an active `M` role by a
  confirmed maintainer's announcement. A repository does not have a lead
  unless such an assertion exists.
- A **co-maintainer** is a confirmed member of the reciprocal maintainer graph.
  Lead and co-maintainer have the same state, merge, and member-action
  authority in ngit.
- A **moderator** may perform issue and proposal management actions, but cannot
  publish repository state or merge.
- A role assignment is an **invitation** until the recipient's own announcement
  acknowledges a confirmed member. Invitees are discovered but their events
  are not authoritative.

The selected maintainer and lead maintainer are independent. Selecting a
different coordinate can produce a different discovered or confirmed graph;
it does not change the lead asserted on the wire.

## Normal Workflows

### Create a repository

```bash
ngit init --name "My Project" --description "What it does" --defaults --json
```

Without `--lead-maintainer`, the publisher is emitted as an untimed `m` and the
repository has no lead. `ngit init` writes the publisher's coordinate to
`nostr.repo`, points `origin` at the publisher's `nostr://` URL, and returns
that URL for sharing.

The publisher can explicitly make themselves lead:

```bash
ngit init --lead-maintainer <own-npub> --json
```

This keeps the resolved maintainer listing and emits the publisher as `M`.

### Invite another maintainer

The publisher replaces their own maintainer listing with themselves and the
npubs passed to `--other-maintainers`:

```bash
ngit repo edit --other-maintainers <bob-npub> --json
```

The listed pubkey is invited until it publishes an announcement. If the
publisher already asserted a lead and that pubkey remains in the resolved
listing, the assertion is inherited without repeating `--lead-maintainer`.
Otherwise every maintainer is emitted as `m`.

### Accept a maintainer invitation

```bash
ngit repo accept --json
```

`repo accept` publishes the invitee's own repository announcement. With a
usable explicit lead, an ordinary invitee lists only themselves and that lead
and reasserts the lead as `M`. If the invitee is themselves the asserted lead,
they retain the complete consolidated membership. Without a usable lead, the
command lists the invitee and the sole confirmed maintainer, falling back to
the selected maintainer for an ambiguous graph.

The push path can perform the same acceptance automatically before publishing
the invitee's first repository state event.

### Update or remove maintainers

`--other-maintainers` supplies the publisher's desired listing. The publisher
is always inserted first; omitted members are removed from that publisher's
announcement. Role generation closes omitted members' active `M` or `m`
records in that announcement.

There is no general before/after authorization preview for this operation. A
special safety guard applies when `--lead-maintainer` names somebody other
than the publisher; its current behavior is documented under Leadership
Designation.

### Leave a repository

```bash
ngit repo leave --json
```

Leaving closes every active `M`, `m`, and `o` entry naming the publisher in
their own announcement. It removes the publisher from that announcement's
typed maintainer and moderator sets and from the deprecated `maintainers`
degradation tag.

An unaccepted maintainer invitation or unacknowledged moderator assignment has
no role to end. Repeating `repo leave` after an earlier leave produces a
distinct already-ended error. A lead may leave; ngit warns that the repository
may become leadless but does not require `--force` or a prior transfer.

## Roles on the Wire

Indexed role tags have the form:

```text
["M"|"m"|"o", "<pubkey>", <start>, <end>, <start>, <end>, ...]
```

- `M` is lead maintainer.
- `m` is co-maintainer.
- `o` is moderator.
- Fewer than four elements, or an odd number of elements, makes the entry
  currently active.
- A pubkey may have one record per role letter so transitions can retain the
  period spent in each role.

Examples:

```text
["m", "<pubkey>"]                           # active from the beginning
["m", "<pubkey>", "100"]                  # active since 100
["m", "<pubkey>", "100", "200"]         # ended at 200
["m", "<pubkey>", "100", "200", "300"] # active again since 300
```

If any `M`, `m`, or `o` tag is present, the deprecated `maintainers` tag is
ignored entirely. This includes an announcement containing only `o` tags.
Without indexed roles, `maintainers` is the fallback listing. If neither form
adds a maintainer, the announcement author is the sole implicit maintainer.

On publication, ngit emits generated `M` and `m` entries for the typed current
maintainer list and also emits a deprecated `maintainers` tag containing that
same list. Existing `o` tags pass through verbatim except when `repo leave`
closes the publisher's self-role.

## Current Membership Resolution

Each maintainer-authored announcement contributes active `M` and `m` listings
to the recursive maintainer set. Role letters do not change maintainer
authorization: both collapse into the same typed set.

An announcement author who has no role entry naming themselves is implicitly a
maintainer. If role tags name the author but none is an active self-`M` or
self-`m`, the author declines maintainership. This covers a member who ended
their role and a moderator-only author acknowledging only `o`. The
self-declaration takes precedence over active maintainer assignments in other
announcements.

Confirmation grows as a fixpoint rooted at the selected maintainer:

1. The selected maintainer starts confirmed unless their own announcement
   declines maintainership.
2. A candidate must be listed by an already-confirmed maintainer.
3. The candidate's own announcement must list an already-confirmed maintainer.
4. Newly confirmed maintainers extend the frontier until no more candidates
   qualify.

A cycle made only from unconfirmed invitees does not bootstrap itself into
authority. `RepoRef::is_authorized_maintainer` is true only for this confirmed
set.

## Lead Resolution

`RepoRef::lead_maintainer` scans confirmed maintainers' latest announcements
for active `M` entries. It returns a lead only when those entries name one
distinct pubkey that remains in the consolidated maintainer set.

- The assigned pubkey need not have accepted yet; an invited pubkey can be
  displayed as lead.
- `M` from an unconfirmed announcement is ignored.
- Two different active `M` targets produce no lead.
- Announcements without `M` produce no lead.
- A legacy `maintainers` graph never falls back to graph-vote inference, even
  if one pubkey has the unique highest in-degree.

Human output displays role badges when they disambiguate the roster. JSON
continues to expose `lead_maintainer`, but it does not expose whether a lead
was absent, contested, or ignored because its asserting author was
unconfirmed.

## Role History

### Parsing and authorization

The current implementation uses history boundaries only to decide whether an
entry is active now. Ended intervals never grant retroactive authority to
historic state, status, label, subject, or cover-note events. There is no
repository-wide conflict-resolution algorithm for incompatible historical
records.

### Publishing from an existing announcement

History is sourced from the publisher's own prior announcement only. A
publisher does not adopt another maintainer's historical view.

If the prior announcement has no `M` or `m` history, ngit materializes untimed
records from its deprecated maintainer listing before republishing. This lets a
removed legacy member receive an explicit end boundary instead of disappearing
without a record.

For each role letter:

- a continuing untimed member stays untimed;
- a new member starts at the publication timestamp once role tags are in use;
- a removed active member receives an end boundary;
- a returning member appends a new start boundary;
- promotion or demotion closes the old letter and opens the new one at the
  same timestamp;
- ended records under the other maintainer letter remain present;
- moderator records pass through unchanged.

`ngit init --clean` drops unknown tags but deliberately retains role tags so it
does not discard moderators or restart the publisher's known history.

Because each author retains only their own prior record, a replaceable event
published by a legacy or history-unaware client can remove that author's
historical boundaries from the latest event. Other maintainers do not
automatically carry a redundant canonical copy.

## Leadership Designation

```bash
ngit init --lead-maintainer <npub-or-hex> --json
ngit repo edit --lead-maintainer <npub-or-hex> --json
```

The implementation accepts an npub or hexadecimal public key even though the
CLI help describes an npub.

### Naming yourself

Specifying yourself keeps the complete resolved maintainer listing and emits
you as `M`. Every other active maintainer is emitted as `m`.

### Naming somebody else

Specifying another pubkey collapses your announcement's current maintainer
listing to exactly you and the designated lead. The lead is emitted as `M` and
you as `m`. `--other-maintainers` containing any third pubkey is rejected;
`--force` does not override that conflict.

The designated lead's announcement is not modified and does not adopt your
roster or history. The lead need not have an announcement or have accepted
before the designation can be published.

### Current force guard

Without `--force`, ngit examines pubkeys present in your previous announcement
that the `[you, lead]` collapse would omit. It exempts a pubkey when the lead's
own announcement directly lists it and the lead either:

- is already a confirmed maintainer; or
- lists you, so the lead becomes reciprocal when your collapse is published.

Every remaining dropped npub is named in the error and the command suggests
rerunning with `--force`.

This is a conservative direct-cover test, not an exact post-publication graph
simulation:

- it can gate a dropped invitee that did not have authority;
- it can require force even when another surviving reciprocal path would keep
  a maintainer confirmed;
- it describes an unauthoritative lead's listing as absent even when the wire
  contains it;
- it does not compare or require retention of role history.

With `--force`, the collapse proceeds and closes the dropped entries in the
publisher's history. With no previous announcement, there are no previous
outgoing listings to gate.

### Local configuration after designation

After publication, `ngit init` always writes the publisher's coordinate to
`nostr.repo`, builds the `nostr://` URL from the publisher's `RepoRef`, and
creates or rewrites `origin` to that URL. Output reports the publisher's share
and clone URLs. Naming another lead does not migrate the checkout to the
lead's coordinate.

When `maintainers.yaml` already exists and differs, it is rewritten with the
publisher first and the collapsed current listing after them. It is not a
store of role history.

## Leadless Co-maintainership

Leadless operation is the default when no maintainer has emitted `M`. There is
no `--no-lead` flag and no explicit marker distinguishing intentional
co-maintainership from a legacy repository or an incomplete migration.

Every active `m` listing participates in recursive discovery. Reciprocity and
the selected-maintainer fixpoint decide authority. Different selected
coordinates can therefore produce different views of a partially connected
graph.

## Moderators

An active `o` entry in a confirmed maintainer's announcement assigns a
moderator. An `o` entry authored by a moderator, invitee, or outsider does not
assign another moderator.

Assignment is an invitation. A moderator is confirmed when their own latest
announcement:

- has an active `o` self-entry; and
- has an active role entry naming an already-confirmed member.

Moderator confirmation grows as a fixpoint, allowing acknowledgement through
another confirmed moderator. A moderator whose own announcement ends every
self-`o` entry is removed despite another maintainer's continuing assignment.

Confirmed moderators may author status, label, subject, and cover-note events.
They cannot publish kind `30618` state, push protected branches, or merge.

ngit discovers moderator announcements by following `o` assignments and
includes moderator coordinates in proposal and issue tags. It has no command
for assigning a moderator and no command that publishes the initial moderator
acknowledgement. `repo leave` can end an acknowledged moderator role that was
created by another client.

## Coordinates and Local Configuration

A coordinate is `(kind, pubkey, identifier)`. Its pubkey is the selected
maintainer and discovery anchor.

Local resolution priority is:

1. explicit `--repo <REMOTE|NADDR|NOSTR-URL>`;
2. `nostr.repo`;
3. the current branch's tracked `nostr://` remote;
4. `origin` when it is `nostr://`;
5. the sole remaining distinct `nostr://` coordinate;
6. `maintainers.yaml` only when no Nostr coordinate is configured.

Before publishing a repo-scoped event, ngit prints the selected naddr and the
source used to resolve it. The selected maintainer is listed first in proposal
announcement tags but receives no extra authorization.

`ngit init` publishes the signer's announcement and then resets `nostr.repo`
and `origin` to the signer. The dedicated `repo accept` command and push-path
acceptance deliberately leave both untouched so the checkout continues to
resolve from the inviter that can later remove the accepter. There is no
`follow-lead` or selected-coordinate migration command.

`maintainers.yaml` is a legacy coordinate fallback. Its first maintainer is
the selected pubkey when that file is used for discovery.

## Consuming Repository Data

For each author, consolidation first chooses the latest announcement using
NIP-01 addressable-event rules: greatest `created_at`, then lowest event id on
a tie. This prevents an older cached version from retaining a role that the
author ended in a newer event.

The recursive maintainer set contributes a union of relay, clone, and Blossom
infrastructure. Shared metadata (`name`, `description`, `web`, hashtags,
upstream metadata, privacy, and forward-compatible unknown tags) follows the
implemented per-field cascade across maintainer-authored announcements.
Moderator-only announcements are fetched for acknowledgement and leave
self-entries but do not expand membership, contribute shared metadata, or add
infrastructure.

Installed Git remote helpers may service clone URLs in signed maintainer
announcements, subject to Git's transport policy. ngit blocks recursive
`nostr` and internal `fd` helper URLs; `ws` and `wss` remain reserved for GRASP
bases.

## Publishing Repository Data

`ngit init` resolves fields from different sources.

Shared metadata comes from the latest maintainer event:

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
- Git servers;
- Blossom servers.

The publisher's maintainer listing and role history come from their own
announcement. They are not inherited from the selected maintainer or the
repository-wide latest event.

The identifier comes from the existing coordinate and cannot change without
`--force`. The earliest unique commit cascades from the publisher's event, the
consolidated repository, and finally the local root commit.

## Repository Output

`ngit repo` human output separates confirmed maintainers, invited maintainers,
and moderators. It displays selected, lead, co-maintainer, and moderator badges
where applicable and annotates assigned-but-unacknowledged moderators.

`ngit repo --json` retains the existing flat fields and adds:

- `moderators`: every assigned moderator;
- `confirmed_moderators`: the acknowledged subset;
- `members`: one deduplicated object per member.

Each `members` entry contains:

- `pubkey`;
- `role`: `lead`, `co-maintainer`, or `moderator`;
- `status`: `confirmed` or `invited`;
- `source`: `role_tag`, `maintainers_tag`, or `implicit`.

An invited designated lead is reported with role `lead`. A pubkey listed as
both maintainer and moderator appears once as a maintainer.

## Announcement Tag Ordering

Proposal announcement tags are ordered:

1. selected maintainer;
2. other confirmed maintainers;
3. confirmed moderators;
4. invited maintainers and assigned-but-unacknowledged moderators.

Issue and status event coordinate sets also cover moderators, but use an
unordered set.

## Authorization Summary

| Actor | Repository state | Merge | Status/labels/subject/cover note |
| --- | --- | --- | --- |
| Confirmed lead | yes | yes | yes |
| Confirmed co-maintainer | yes | yes | yes |
| Confirmed moderator | no | no | yes |
| Invitee | no | no | no |
| Outsider | no | no | no |

Issue and proposal authors retain the author-specific actions granted by their
event type. This table covers authority derived from repository membership.

## Current Edge-case Behavior

### Legacy announcements

The deprecated `maintainers` tag supplies current co-maintainers only when an
announcement has no indexed role tags. It supplies no lead and no explicit
history. A later role-aware publication materializes untimed history from the
publisher's own legacy listing.

### Partial indexed-role migration

One maintainer publishing `m` tags does not cause another author's legacy
announcement to be parsed as role-aware. Each event independently decides
whether its own `maintainers` fallback applies. There is no repository-level
migration state and no legacy lead inference.

### Conflicting explicit leads

If confirmed announcements name different active `M` targets,
`lead_maintainer` returns none. Read and write operations continue under the
confirmed-maintainer authorization model; there is no conflict-specific
mutation guard.

### A legacy client replaces a role-aware event

NIP-01 selects the newer legacy event for that author. Its indexed history is
no longer available from that author's current announcement. Other
maintainers' events retain only their own historical views, so they are not an
automatic redundant copy.

### Leadership transfer before acceptance

A maintainer can designate an invited pubkey as lead. The publisher's listing
collapses immediately to `[publisher, lead]` if the force guard permits it.
The designated lead's event and local configuration remain unchanged. There
is no pending-transfer state or automatic finalization after acceptance.

### Two repositories with different histories

ngit does not merge or choose between repository-wide historical records.
Current membership is resolved from active entries reachable from the selected
coordinate. Ended history is retained per publisher but is not consulted for
historic event authorization.

### Same identifier, unrelated repositories

Reciprocity prevents a repository from treating an unsolicited invitee's
same-identifier state and member-action events as authoritative. The invitee
must publish an announcement acknowledging the selected confirmed group.

## Current Implementation Invariants

The branch's tests pin these behaviors:

1. Fresh init emits `m` unless the publisher passes `--lead-maintainer`.
2. A lead is read only from a unique authoritative active `M`; graph structure
   never supplies a fallback.
3. Maintainer confirmation grows from the selected maintainer as a reciprocal
   fixpoint.
4. Invited maintainers cannot author authoritative state or member actions.
5. Confirmed moderators can author member actions but cannot publish state or
   merge.
6. An active self-`o` does not imply maintainership, and an ended self-role
   takes precedence over assignments in other announcements.
7. `repo accept` under a lead lists only the accepter and the lead; an
   accepting lead keeps the full consolidated roster.
8. Setting another lead collapses the publisher's listing immediately and uses
   a conservative direct-cover `--force` gate.
9. Each publisher preserves only the history in their own previous
   announcement.
10. History boundaries affect current activeness but not retroactive event
    authorization.
11. `ngit init` keeps local selection anchored to the signer even when it
    designates somebody else as lead.
12. `repo leave` ends the publisher's active self-roles and permits a lead to
    leave with a warning.
