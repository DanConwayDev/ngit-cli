# Maintainer Model

> **Proposal:** this document defines the intended maintainer model and public
> CLI. It differs from both the pre-role model and parts of the current
> indexed-role implementation. The two preceding revisions of this file record
> those earlier snapshots.

This document has three layers:

- **Understand the maintainership model** starts with roles and everyday
  commands, then explains maintainer-scoped addresses and selected
  maintainers.
- **Protocol model** specifies the wire format, graph resolution, replicated
  history, and lead resolution.
- **Guidance for clients** defines safe defaults, edge-case handling, and the
  protections required before publishing membership changes.

## Understand the Maintainership Model

### The normal model

Most repositories have one lead maintainer and, when needed, one or more
co-maintainers. ngit resolves the lead automatically during discovery, so the
coordinate and forwarding details later in this document rarely affect the
ordinary workflow.

The roles are:

- **Sole maintainer:** creates the repository. No role option or role tag is
  needed while nobody else is involved.
- **Lead maintainer:** coordinates the roster and provides the normal place to
  clone. The lead has no additional signing or merge authority.
- **Co-maintainer:** has the same power as the lead to publish Git state,
  merge, and manage the repository.
- **Invitee:** has no maintainer authority until they accept. Joining always
  requires statements from both sides.
- **Moderator:** may help manage issues and proposals, but cannot publish
  repository state or merge.

The usual path is simple: Alice creates a repository, invites Bob, becomes lead
automatically as the inviter, and Bob accepts as a co-maintainer.

### Normal two-person workflow

#### 1. Alice creates the repository

```bash
ngit init --name "My Project" --description "What it does" --defaults
```

Alice is the only maintainer. No lead or maintainer options are needed yet.

#### 2. Alice invites Bob and establishes the normal lead

```bash
ngit repo edit --add-maintainer <bob-npub>
```

Because this is the first invitation from a sole-maintainer repository, ngit
automatically records Alice as lead. Bob is visible as invited but has no
maintainer authority until he accepts. Alice would add
`--no-lead-maintainer` only to choose the exceptional leadless model.

#### 3. Bob accepts

After selecting or cloning Alice's repository, Bob runs:

```bash
ngit repo accept
```

Bob accepts only the offered co-maintainer role. He cannot use acceptance to
make himself lead, add another person, or replace Alice's relationships. Once
his announcement is published, he has the same repository authority as Alice.

#### 4. Alice records Bob's acceptance

The next time Alice runs an ngit command in the repository, ngit sees Bob's
acceptance and prints persistent guidance:

```text
Bob accepted maintainership at <time>.
record this in your replicated membership history with:
  ngit repo edit --acknowledge-maintainer-change <bob-npub>
```

Alice runs the displayed command:

```bash
ngit repo edit --acknowledge-maintainer-change <bob-npub>
```

That republishes Alice's announcement with Bob's effective start changed from
the invitation time to the acceptance time. ngit does not wait for Alice to
make an unrelated announcement edit; those may be rare, and Bob could later
leave or delete his own announcement. Every later command repeats the guidance
until Alice records the change. No command opens a confirmation prompt or
publishes the acknowledgement automatically. JSON output instead includes
`pending_history_acknowledgements` with the pubkey, transition, time, and exact
command.

#### 5. Every maintainer retains the history

Bob's acceptance already records his own start. Alice and every other
confirmed maintainer whose announcement does not yet contain that transition
are shown the same `--acknowledge-maintainer-change <bob-npub>` guidance. Each
runs it to retain a redundant historical view. The same rule applies when a
maintainer is removed or leaves; history replication is not a lead-only job.

#### 6. The repository continues normally

Alice or Bob can publish state, merge, and perform ordinary maintainer actions.
A confirmed maintainer may invite another person, subject to the repository-join
checks described below. The normal UI should direct routine membership
coordination through the lead to avoid crossed invitations.

### Everyday commands

| Intent | Command |
| --- | --- |
| Invite one maintainer | `ngit repo edit --add-maintainer <npub>` |
| Remove one relationship | `ngit repo edit --remove-maintainer <npub>` |
| Choose a lead | `ngit repo edit --lead-maintainer <npub>` |
| Make a leadless membership change | add/remove plus `--no-lead-maintainer` |
| Accept an invitation | `ngit repo accept` |
| Record a discovered transition | `ngit repo edit --acknowledge-maintainer-change <npub>` |
| Leave a repository | `ngit repo leave` |
| Follow the lead's repository coordinate | `ngit repo follow-lead` |

`--add-maintainer` and `--remove-maintainer` change one relationship at a time.
There is no public option for replacing the complete maintainer list. This
makes an omitted name impossible to mistake for an intentional removal.

When the selected view resolves an explicit lead, ordinary add and remove
commands preserve that lead automatically. The flag does not need to be
repeated.

The first `--add-maintainer` in a sole-maintainer repository automatically
assigns the publisher as lead. Passing `--no-lead-maintainer` opts out of that
default.

Once a repository is explicitly leadless, **every** add or remove must include
exactly one governance choice:

```bash
--lead-maintainer <npub>  # recommended normal model
--no-lead-maintainer      # affirm no lead for this one mutation
```

Older ngit repositories used a legacy model that could list maintainers but
could not directly express a lead or retain maintainership history. ngit keeps
those repositories working. Their first new membership change must make the
inferred lead explicit or deliberately choose no lead. Omitting both choices
fails with an actionable error:

```text
a lead maintainer must be assigned for this membership change
choose one with: --lead-maintainer <npub>
or keep the repository leadless with: --no-lead-maintainer
```

The error may recommend the legacy-inferred lead when there is one. A previous
`--no-lead-maintainer` records the wire state but is not reused as consent for
a later roster change. The flag cannot remove an existing lead or override a
pending or conflicting lead path. “Legacy migration” describes the automatic
compatibility update in protocol detail.

The governance choice may accompany one relationship action:

```bash
ngit repo edit \
  --add-maintainer <carol-npub> \
  --lead-maintainer <alice-npub>
```

Every mutation accepts `--json` for machine-readable previews, results, and
errors.

### Adding a maintainer

```bash
ngit repo edit --add-maintainer <carol-npub>
```

If a lead is already explicit, it is preserved. Carol is invited until her own
announcement acknowledges the repository. When she accepts, every confirmed
maintainer is shown the acknowledgement command as they next use ngit.

An add can do more than expected when Carol already has a same-identifier
repository. ngit therefore previews the resulting membership and Git state.
It refuses instead of silently joining repositories; the detailed rule is in
“Adding a maintainer can join repositories.”

### Removing a maintainer

```bash
ngit repo edit --remove-maintainer <bob-npub>
```

This withdraws only the publisher's relationship to Bob. In the normal
lead-shaped structure, the lead's withdrawal removes Bob. Replicated history in
other announcements is historical only and cannot keep him active.

If Bob remains active through a real relationship elsewhere, or the withdrawal
would also disconnect Carol, ngit refuses and names the relevant people and
paths. The operator must add a relationship that should retain them or remove
each intended person explicitly. There is no `--force` shortcut for combining
several membership decisions.

Once the removal is observed, other maintainers are shown the command that
records Bob's end time. Their previously open historical copies never delay
the current removal.

### Inspecting a repository

```bash
ngit repo
ngit repo --json
```

Repository output should answer four user-level questions before exposing wire
details:

1. Which maintainer coordinate did this checkout select?
2. Who is the lead, and is that explicit, legacy-inferred, or unresolved?
3. Who is confirmed, invited, or a moderator?
4. Are there pending history acknowledgements or a safer lead coordinate to
   follow?

Seeing “selected: Alice” and “lead: Bob” is not itself an error. It normally
means the checkout still uses Alice's entry point after a leadership handover.
`repo follow-lead` verifies whether it is safe to switch.

### Leaving a repository

```bash
ngit repo leave
```

Leaving publishes an ended self-role. Because a maintainer's self-role takes
precedence, Alice continuing to list Bob does not make Bob active after Bob
leaves. Bob returns only by publishing a later self-role start that
acknowledges the repository again. If leaving would also disconnect other
maintainers, ngit refuses and explains which relationships must be dealt with
first.

Bob's signed self-role end is the preferred history boundary. If departure is
instead expressed by a signed deletion request for his announcement, its
`created_at` is the best available end time. If a departure is established but
neither signed boundary is available, a maintainer records the earliest
well-supported observation of Bob's disappearance as an estimated end. Clients
label that boundary as estimated and replace it if stronger signed evidence is
later found. Failure to fetch from one relay is not departure evidence.

### Transferring the lead

The target first joins as an ordinary confirmed maintainer. Alice then records
her view of the handoff:

```bash
ngit repo edit --lead-maintainer <bob-npub>
```

Alice's coordinate now points to Bob. Bob may still point to Alice, creating a
temporary two-coordinate loop that clients show as a pending handover. Bob
completes it by pointing to himself:

```bash
ngit repo edit --lead-maintainer <bob-npub>
```

The chain is now Alice → Bob → Bob, so Bob is the resolved lead. A
co-maintainer who still points to Alice reaches Bob through Alice and need not
republish merely to repeat the handover. Before Bob can point to himself, ngit
requires Bob to retain the complete roster and resolved history so completing
the forward cannot drop people or state.

Once Bob is the resolved lead, clients may suggest:

```bash
ngit repo follow-lead
```

That command switches the local `nostr://` coordinate only after verifying
that Bob's rooted view contains the same membership and Git state.

### Deliberately using no lead

Leadless governance is an advanced alternative for the small number of
repositories that deliberately do not want a coordinator. The absence of a
lead must be affirmed on every membership change:

```bash
ngit repo edit \
  --add-maintainer <bob-npub> \
  --no-lead-maintainer

ngit repo edit \
  --remove-maintainer <bob-npub> \
  --no-lead-maintainer
```

The per-command flag makes the absence of a lead deliberate rather than an
accidental omission. It does not grant different authority or weaken the
reciprocity and repository-state checks. Converting an existing lead
repository to leadless governance requires a separate future transition
workflow because each maintainer's signed lead statement must be resolved
safely.

Leadless governance changes only how clients resolve forwarding, recommend a
clone coordinate, and display the repository. It does not change
authorization: `M` and `m` maintainers have the same powers, and the reciprocal
confirmed graph remains authoritative with or without a lead.

A leadless repository has no single recommended maintainer coordinate. Users
should know which `alice/my-repo`-style entry point they selected, because two
entry points can expose different views during a membership or relay partition.
It does not receive follow-the-lead warnings or switching guidance.

### Why a repository address names a maintainer

A centralized forge can keep an organization registry and transfer ownership
of `org/my-repo` from one account to another. Nostr has no provider or central
registry that can perform that handover. Repository announcements are signed
by individual keys, so their addresses include both the signer and repository
identifier.

The human shorthand `alice/my-repo` therefore means “start with Alice's signed
announcement for `my-repo`.” A real URL looks like:

```text
nostr://<alice-npub>/my-repo
```

Alice's pubkey permanently owns the `alice/my-repo` coordinate: only somebody
who can sign as Alice can replace that announcement. The protocol cannot
transfer exclusive control of Alice's coordinate to Bob. Alice can instead
publish a lead pointer to Bob, making her coordinate act as a persistent,
signed forward to `bob/my-repo` while that pointer remains in her latest
announcement.

This forwarding is the normal case and usually requires no user decision. If a
checkout still selects Alice after she points to Bob, ngit warns after every
repository command and offers `ngit repo follow-lead`. It never rewrites the
checkout silently.

The forward is persistent but revocable. Alice can later point back to herself
or to Carol, stopping new and unswitched users of `alice/my-repo` from being
directed to Bob. She cannot pull back checkouts that already followed Bob and
now select `bob/my-repo`.

The maintainer named by the URL is the **selected maintainer**. This describes
the discovery entry point and the key controlling that coordinate; it does not
grant stronger repository or merge authority. During partial relay views or a
handover, starting from Alice and Bob can temporarily expose different state,
so clients show the selected maintainer and resolved lead separately.

### An organization pubkey is not a forge organization

A repository may use a pubkey labelled `org` as its selected maintainer or
lead, producing the familiar-looking shorthand `org/my-repo`. Its keypair may
indeed be administered centrally by an organization. The repository protocol
still sees one public key controlling one announcement coordinate.

A forge can transfer an organization account and revoke the old operator's
access. Nostr cannot prove that somebody deleted a secret key. Giving control
of an organization pubkey to a new operator does not prevent the old operator
from retaining the secret and continuing to produce valid signatures. A team
can use external shared-signing or key-custody arrangements, but those are not
an exclusive ownership transfer provided by the repository protocol.

The safer protocol-level handover is the normal lead forward: establish a new
pubkey as lead and let clients explicitly follow it. The old organization
coordinate remains controlled by every party that still possesses its secret
and can later revoke or redirect its own forward.

## Protocol Model

### Design principles

1. **Reciprocity defines the repository.** A pubkey cannot make another
   person's same-identifier events authoritative merely by listing them.
2. **Lead is semantic.** `M` is a signed forwarding pointer used for
   coordination. It does not replace the reciprocal graph or grant stronger
   signatures.
3. **The API expresses intent.** ngit changes one relationship and preserves
   unrelated edges, intervals, metadata, and unknown tags.
4. **Current edges and historical copies are distinct.** Both use the NIP-34
   role record, but an explicit `open` sentinel marks a retained open interval
   as historical-only so it never authorizes its subject.
5. **Membership writes fail closed.** An unexpected graph join, partition, or
   state replacement is not published.
6. **Signed disagreement remains visible.** Other clients may omit or rewrite
   data in their own replaceable events; resolution needs explicit precedence
   and must preserve conflicting copies for audit.

### Terms

- The **selected maintainer** is the pubkey in the `nostr://` URL,
  `nostr.repo`, naddr, or explicit `--repo` coordinate where discovery starts.
- A **maintainer listing** is an active `M` or `m` tag, or an entry in the
  legacy `maintainers` fallback.
- A **confirmed maintainer** is admitted by the reciprocal fixpoint rooted at
  the selected maintainer.
- An **invited maintainer** is listed by the discovered graph but has not made
  the acknowledgement needed to join it.
- The **maintainer graph** is the directed set of current maintainer listings.
- The **virtual repository** is the confirmed component obtained from that
  graph for one identifier and one selected coordinate.
- A **history view** is one announcement author's replicated account of
  effective roles over time. Historical-only records never contribute current
  graph edges.

Two coordinates with the same identifier can initially describe unrelated
virtual repositories. They become one only when reciprocal relationships join
their confirmed components. This matters because kind `30618` state events do
not carry a complete repository coordinate that could otherwise disambiguate
same-identifier repositories.

### Current role tags

Indexed role tags have this form:

```text
["M"|"m"|"o", "<pubkey>", <start>, <end>, <start>, <end>, ...]
```

- `M` points from this announcement toward the author's lead maintainer. A
  lead points `M` to themselves, terminating the forwarding walk.
- `m` means the author regards the subject as a co-maintainer.
- `o` assigns or acknowledges a moderator.
- With ordinary numeric boundaries, a tag is active when it has fewer than
  four elements or an odd number of elements: its final boundary is a start.
- A literal `open` in an end position retains a last-known-open interval as
  historical-only. Its even-length tag is inactive for current assignment.
- A pubkey may have one record for each role letter. A role transition closes
  the old letter and starts the new one rather than rewriting the past.

Examples:

```text
["m", "<pubkey>"]                           # active, start unknown
["m", "<pubkey>", "100"]                  # active since 100
["m", "<pubkey>", "100", "200"]         # ended at 200
["m", "<pubkey>", "100", "200", "300"] # active again since 300
["m", "<pubkey>", "300", "open"]        # historical copy, not active
```

The author is part of the fact. Alice's `m:Bob` is Alice's outgoing
relationship to Bob. Bob's `m:Bob` is Bob's acknowledgement of his own role.
The same subject in two events is not one shared record.

`M` and `m` create the same kind of maintainer edge and have identical
authorization weight. A role-aware multi-person announcement includes an
active `M` or `m` self-role so leaving can be stated unambiguously.

The normal lead-shaped topology follows the sibling NIP-34 draft: the lead has
an active self-`M` and actively lists the current roster, while co-maintainers
point `M` toward the lead and actively list only themselves and that lead.
Co-maintainers may retain everybody else's intervals as inactive numeric-ended
or `open` historical records. This convention lets the lead remove somebody
without redundant history defeating the removal. If a co-maintainer
deliberately publishes another active relationship, it is a real graph edge
and can retain that person or join another component.

If an announcement has no `M`, `m`, `o`, or legacy `maintainers` tag, its
author is the implicit sole maintainer. This is the preferred one-person wire
format.

If any `M`, `m`, or `o` tag is present, the deprecated `maintainers` tag in that
announcement is ignored. Otherwise `maintainers` supplies legacy listings.
Role-aware publishers may still emit a degradation `maintainers` tag containing
their current accepted `M` and `m` subjects for older clients. Invitations are
carried by indexed roles and are not asserted as accepted in that fallback.

The happy-path announcements progress like this. Alice's first invitation at
`T1` automatically establishes Alice as lead and creates Bob's pending edge:

```text
["M", "<alice-pubkey>", "T1"]
["m", "<bob-pubkey>", "T1"]
["maintainers", "<alice-pubkey>"]
```

Bob's acceptance at `T2` lists only himself and the lead as active:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>"]
```

When Alice acknowledges the acceptance, her replacement changes Bob's first
start from `T1` to the effective start `T2` and adds Bob to her accepted
degradation fallback. Her active `M:Alice` record remains otherwise unchanged.

### Replicated role history

The role tags in the sibling NIP-34 draft already carry start/end history and
define precedence between conflicting copies. Effective membership history is
therefore replicated in `M`, `m`, and `o`; it does not need another tag type.

The missing distinction is how a maintainer can retain a still-open interval
for somebody they are not currently assigning. This proposal reserves the
literal `open` in an end position:

```text
["m", "<bob-pubkey>", "200", "open"]
```

This says: “my historical view has Bob confirmed from time 200 with no known
end, but this record is not my current assignment.” The record has an even
number of elements, so current ngit and clients following the existing parity
rule treat it as inactive. A numeric end replaces `open` when the end is
observed:

```text
["m", "<bob-pubkey>", "200", "300"]
```

The sibling NIP-34 draft must be clarified so a valid history boundary may be
this literal sentinel as well as a Unix timestamp. Clients must never parse
`open` as an end time or as an active role assignment.

When the publisher has a real current relationship to the subject, the
ordinary omitted end remains active and carries that history directly. A
publisher uses `open` only for a copied interval that must not create a graph
edge. If a historical-only relationship later becomes a real edge, the
publisher replaces the sentinel with the transition time and opens the real
interval at that same time.

Role-aware publishers preserve the resolved histories they know. When they
observe a confirmed start or end missing from their own record, every ngit
command reports the change and shows
`--acknowledge-maintainer-change <npub>`. Acknowledging updates historical
boundaries; it does not add, remove, or accept a current relationship.

An invitation is not effective maintainership. Alice initially publishes
`m:Bob` with `T1` so the invitation is discoverable. Once Bob accepts at `T2`,
Alice changes the beginning of that first interval to `T2`: she is now signing
her observation that Bob became a maintainer then, rather than claiming he was
one from the invitation time. Other maintainers without a real Bob edge retain
the same effective start using the inactive `open` form:

```text
T1  Alice publishes m:Bob,T1             Bob is invited
T2  Bob publishes his acceptance         Bob becomes confirmed
T3  Alice records m:Bob,T2                real edge, accepted start retained
T4  Carol records m:Bob,T2,open           historical copy, no current edge
```

Alice sees the acknowledgement command at the first ngit command that observes
Bob's acceptance, not the next unrelated announcement edit. Bob's acceptance
event and self-role provide the evidence for `T2`.

An explicit role end in the announcement that causes a transition supplies the
strongest departure boundary. When the departing author's announcement
disappears without such an end, a signed deletion request supplies its
`created_at`. If departure is otherwise established, the first well-supported
observation of disappearance is recorded as an estimate rather than leaving
the end empty. Clients distinguish that estimate from signed evidence and ask
maintainers to correct their copies when stronger evidence appears. Missing
data from one relay is never enough to infer an end.

#### History precedence

History needs deterministic precedence because replaceable events can omit or
contradict copies. This proposal retains the sibling NIP's rule:

1. The selected maintainer's present record is authoritative when it exists.
2. Otherwise use the record at the shortest confirmed-graph distance from the
   selected maintainer.
3. Break equal-distance ties by lowest author pubkey.
4. Conflicting records remain visible even when precedence chooses the
   resolved view.

In the normal lead-shaped graph the selected maintainer is usually the lead.
When a checkout still selects a co-maintainer, that co-maintainer actively
lists the lead, so the lead's fuller record is normally the nearest fallback.

An omitted record does not erase history retained elsewhere. An explicit
replacement interval corrects the preferred view; omission falls through to a
retained copy.

This precedence applies only to historical display and past-event filtering.
Current authority always comes from active role edges in the reciprocal graph.
An `open` historical copy is inactive, regardless of which author published
it.

When a lead removes Bob in the ordinary lead-shaped graph, the lead closes the
real edge and effective interval at the removal time. Bob leaves immediately.
Other maintainers may still carry `open` historical copies until they
acknowledge the removal; those copies cannot delay it. A real alternative edge
can retain Bob and causes the removal command to fail, as described above.

A leadership transfer cannot complete while the proposed lead's history is
missing resolved transitions. The new lead first acknowledges the retained
view. Repositories with conflicting histories require the same explicit
reconciliation workflow as conflicting Git state.

### Reciprocal graph resolution

Clients select the latest addressable announcement for each author using
NIP-01 ordering, then resolve current membership as a fixpoint rooted at the
selected maintainer:

1. The selected author starts confirmed unless their own announcement records
   that their self-role ended.
2. A candidate must be actively listed by an already-confirmed maintainer.
3. The candidate's latest announcement must actively list an
   already-confirmed maintainer and must not end the candidate's self-role.
4. Newly confirmed maintainers extend the frontier until it stops growing.

Reciprocity is at the component level, not limited to isolated pairs. A cycle
of unconfirmed invitees cannot bootstrap itself into authority. A unilateral
listing discovers an invitation but does not import the subject's state.

If Bob is reciprocally connected to Alice and also to Tom, Bob intentionally
joins those same-identifier components. Alice, Bob, and Tom are then one
virtual repository. If Tom merely lists Bob and Bob acknowledges nobody in
Tom's component, Tom's listing does not merge them.

Current kind `30618` state and maintainer-only NIP-34 actions are filtered
through the confirmed component. Starting from another coordinate can expose a
different component while the graph is partitioned.

### Lead resolution

Lead resolution is a semantic forwarding walk over the confirmed graph. It
starts at the selected maintainer and follows that announcement's one active
`M` target. If the target points onward, resolution continues. A confirmed
maintainer whose active `M` points to themselves is the terminal lead.

```text
Carol → Alice → Bob → Bob
                       ^ terminal lead
```

This lets a co-maintainer's old coordinate follow later handovers without that
co-maintainer republishing every transition. It also preserves coordinate
ownership: replacing Alice's pointer immediately changes where
`alice/my-repo` leads, but cannot change a checkout that already selects Bob.

Resolution has seven results:

1. **Implicit sole.** A one-person component with no role or legacy tags treats
   its author as workflow lead without emitting `M`.
2. **Explicit.** The selected pointer chain reaches a confirmed maintainer with
   an active self-`M`. That pubkey is lead.
3. **Legacy inferred.** With no active `M` at the selected announcement and a
   selected announcement that has not adopted indexed roles, every active
   directed listing between two distinct confirmed maintainers is one vote for
   its subject. The unique highest positive count is inferred as lead.
4. **Explicit none.** The selected maintainer published indexed `m` roles
   without `M` through the no-lead workflow. Legacy inference is disabled for
   that rooted view.
5. **None.** A legacy graph has no positive unique vote winner.
6. **Pending.** A pointer target has not accepted, its announcement is missing,
   or the handover temporarily forms a multi-pubkey cycle such as
   Alice → Bob → Alice.
7. **Conflict.** An announcement supplies multiple active lead targets, a
   target has explicitly left, or the pointer walk otherwise cannot produce
   one safe continuation.

Legacy vote inference remains while the selected announcement is legacy. Edges
from partially migrated members still contribute to its vote count, so one
co-maintainer adopting `m` does not erase an established lead. Once the
selected maintainer deliberately publishes indexed `m` without `M`, inference
stops for that rooted view. This makes `--no-lead-maintainer` expressible while
preserving repositories that have not opted into the new role model.

The Alice/Bob happy path begins Bob → Alice → Alice. After handover it becomes
Alice → Bob → Bob. A tied legacy co-maintainer topology remains leadless and
continues to authorize all reciprocal members.

Active `M` statements outside the selected pointer path do not create a global
conflict merely because they name another lead. They affect views rooted at
coordinates that reach them. Historical `M` records ending in `open` are
inactive and never enter the walk.

Human and JSON output expose the source:

```json
{
  "lead_maintainer": "npub1...",
  "lead_source": "explicit"
}
```

`lead_source` is `implicit_sole`, `explicit`, `legacy_inferred`,
`explicit_none`, `none`, `pending`, or `conflict`. JSON also exposes
`lead_path`, ordered from the selected maintainer through the terminal or
failed target.

## Guidance for Clients

The protocol permits temporary disagreement and unusual graph shapes. Clients
should make the ordinary lead workflow feel simple while keeping the selected
coordinate, inferred state, and exceptional consequences visible.

### Recommended defaults

- Show **selected maintainer** as both the current entry point and the signer
  controlling that coordinate. Show **lead maintainer** separately and do not
  present the lead as owner of every maintainer's coordinate.
- When the selected announcement's pointer walk resolves to another confirmed
  lead with equivalent graph and state, warn after every human-facing
  repository command and show `ngit repo follow-lead` as the remedy. The clone
  path should emit the same warning immediately.
- Never rewrite the local coordinate automatically. After an explicit follow,
  the checkout is anchored at the new selected coordinate and is no longer
  affected by the old signer's later redirects.
- Stop recommending an old target as soon as the selected signer withdraws or
  changes that pointer. Do not follow a target named only by unrelated
  announcements.
- A deliberate leadless repository has no forward and receives no
  follow-the-lead warning.
- On the first add from a sole-maintainer repository, assign the publisher as
  lead automatically unless `--no-lead-maintainer` is supplied.
- On every add or remove in an explicitly leadless repository, require exactly
  one of `--lead-maintainer <npub>` or `--no-lead-maintainer`. Require the same
  choice when a multi-maintainer legacy view has no explicit lead declaration.
- Present a unilateral listing as an invitation. Do not call the invitee a
  maintainer or accept their state until reciprocity confirms them.
- Report a newly confirmed start or end as soon as it is observed and repeat
  the exact `--acknowledge-maintainer-change` command until it is run. Do not
  open an interactive prompt or publish the acknowledgement automatically.
- In JSON mode, expose structured pending actions and exact commands. Include
  `lead_path`,
  `recommended_coordinate`, and `follow_lead_command` on every response where
  a forward is available.
- Show a lead conflict or legacy-inferred lead honestly; do not silently choose
  a convenient explicit lead.

The repeated human warning should be short and actionable:

```text
this checkout uses alice/my-repo; its lead pointer resolves to bob/my-repo
switch to the lead with: ngit repo follow-lead
```

It remains advisory: the requested command continues, and Alice's coordinate
is still a valid user-selected trust root. The warning stops after following,
if Alice withdraws the pointer, or if the pointer no longer resolves safely.

### Required safety posture

Membership operations fail before publication when they would:

- remove or add people beyond the named action;
- withdraw an invitation as an accidental side effect;
- connect another same-identifier maintainer component;
- select a different earliest unique commit or fork relationship;
- make a different kind `30618` state authoritative; or
- make branches or tags appear, disappear, or change OID unexpectedly.

Errors name the affected people, coordinates, and refs. A repository merge or
several removals are separate decisions, not meanings assigned to `--force`.

### Membership mutation contract

Kind `30617` is replaceable and contains a complete event body, but the API
applies one relationship delta plus, when required, one explicit governance
choice. Before signing, every membership mutation must:

1. fetch the latest announcements and state reachable from the selected
   component and every named pubkey;
2. preserve unrelated roles, relationship intervals, replicated history,
   metadata, unknown tags, and personal infrastructure;
3. construct the proposed replacement event in memory;
4. resolve confirmed maintainers and moderators before and after the change;
5. resolve repository identity and state, including the `r` earliest unique
   commit, informational `u` fork links, and every Git ref/OID;
6. display the intended and consequential changes;
7. fail if the fetched predecessor changes before publication; and
8. publish and verify that the resulting graph matches the preview.

The comparison reports:

- newly invited and newly confirmed pubkeys;
- whether the named removed pubkey stays confirmed through another path;
- every additional maintainer or moderator gained or lost;
- invitations withdrawn;
- component joins and partitions;
- lead resolution changes;
- pending history acknowledgements;
- a different earliest unique commit or `u` relationship;
- competing kind `30618` state and refs that would appear, disappear, or
  change OID; and
- selected-coordinate changes.

Ordinary success requires that graph effects match the command's name and no
conflicting state is selected. Human errors and JSON output expose exact npubs,
refs, and coordinates.

### Adding a maintainer can join repositories

`--add-maintainer Bob` is not always just an invitation. Bob may already have a
same-identifier announcement that lists Alice and Tom. Alice's new edge can
confirm Bob immediately, import Tom's reciprocal component, and make its kind
`30618` events authoritative in Alice's view.

Preflight distinguishes:

1. **Unacknowledged invitation.** Bob does not acknowledge Alice's component.
   Only an invitation is added.
2. **Expected confirmation.** Bob already acknowledges the same component and
   no extra member, identity, history, or state conflict enters. The preview
   reports that Bob becomes confirmed.
3. **Repository join.** The operation adds an unexpected confirmed pubkey,
   connects another component, changes identity or fork metadata, introduces
   conflicting history, or changes resolved refs. It fails before publishing.

A join error shows both components, the connecting edges, history differences,
and a ref-by-ref state comparison. It never suggests `--force`. Operators must
first reconcile Git history, membership history, and the signed repository
state, then use a dedicated repository-merge workflow. Until that workflow
exists, ngit conservatively refuses the join.

### Accepting with an existing repository

`repo accept` replaces the invitee's own kind `30617` event for that author and
identifier. It may also join reciprocal components. This is dangerous when Bob
already uses the identifier for an experimental fork with his own `u` upstream
tag, earliest unique commit, membership history, or kind `30618` state.

Before acceptance, ngit resolves:

- the inviting component from the selected coordinate; and
- Bob's same-identifier component, announcement metadata, history view, and
  state before replacement.

It previews the post-acceptance graph and existing state-selection result. It
shows whether Bob's fork relationship, earliest unique commit, history, or
refs would be retained, replaced, or imported into the joined component.

If either repository's state would displace the other's refs, acceptance ends
with a destructive-consequence warning and a non-zero error:

```text
cannot accept this maintainer invitation safely

your existing same-identifier announcement describes an experimental fork:
  u                         <bob-upstream-coordinate>
  earliest unique commit    <bob-root-commit>
the inviting repository uses earliest unique commit <alice-root-commit>.
accepting would replace your announcement, join the two maintainer graphs,
and select the inviting repository state:
  refs/heads/experiment  <bob-oid>  would no longer be in resolved state
  refs/heads/main        <old-oid>  would become <alice-oid>

reconcile the repositories and their state before accepting; --force is not
available for a repository merge
```

The reverse direction is reported when Bob's state would displace Alice's.
“Replace” means disappear from the resolved view; relay retention of the old
event is not a recovery guarantee.

Acceptance is safe when Bob has no same-identifier repository, is already in
the same component, or both components, identity metadata, histories, and
state are compatible. Cosmetic metadata can follow the ordinary shared-field
rules. Conflicting identity, `u`, history, or refs block.

### Lead declaration safety

`--lead-maintainer <npub>` changes the publisher's semantic lead statement. If
the publisher names themselves, they keep the full active roster. If they name
somebody else, the normal sibling NIP-34 shape makes the publisher a
co-maintainer: only the publisher and proposed lead remain active in their
announcement, while every other role interval is retained as ended or
historical-only.

This convention does not give the target protocol ownership of the roster,
but it can change the reciprocal graph. The target of a standalone transfer
must already be confirmed, and the normal handoff is “add, accept, acknowledge
history, let the proposed lead cover the roster, then declare.”

ngit therefore simulates the exact post-declaration graph. If Carol is
confirmed only because Alice currently lists her, changing Alice into the
co-maintainer shape would remove Carol unless Bob first creates a retaining
edge. The same rule protects outstanding invitations.

That operation fails even if an earlier implementation offered `--force`:

```text
cannot declare <bob-npub> as lead because it would remove confirmed
maintainers from the repository graph:
  <carol-npub>
  <dave-npub>

it would also withdraw these maintainer invitations:
  <eve-npub>

ask <bob-npub> to add each maintainer first, or explicitly remove them before
changing the lead:
  ngit repo edit --remove-maintainer <carol-npub>
  ngit repo edit --remove-maintainer <dave-npub>
  ngit repo edit --remove-maintainer <eve-npub>
```

The error names every confirmed or invited person. Asking Bob to add them
establishes another retaining path; removing them first records Alice's intent.
A generic force would conflate leadership semantics with membership removal.

### Legacy migration

Unrelated metadata changes do not migrate membership. A description edit must
not silently reinterpret a working legacy graph.

- A sole legacy author continues with no membership tags. Their first add
  automatically makes them lead unless they pass `--no-lead-maintainer`.
- A unique listing-vote winner remains `legacy_inferred`. The next membership
  action normally materializes that pubkey with `--lead-maintainer` while
  preserving every edge. `--no-lead-maintainer` adopts indexed `m` without
  `M`, deliberately ending inference in the selected rooted view.
- A tied leadless legacy graph remains usable. Its next membership action must
  either begin explicit lead convergence or affirm leadless governance.
- Every later add or remove that remains leadless repeats
  `--no-lead-maintainer`; the indexed no-lead wire state does not waive this
  API safeguard.
- Each existing explicit `M` pointer is preserved unless its signer deliberately
  changes it.
- A pending or conflicting pointer path rooted at the selected coordinate
  blocks lead and membership mutations that depend on resolving that path.
  Different `M` targets elsewhere in the component are not by themselves a
  conflict.
- Converting an untimed legacy edge never invents a historical timestamp.
  Effective history starts only from observed evidence and otherwise remains
  unknown.

The membership mutation that adopts indexed roles republishes the selected
announcement with `M`, `m`, and any preserved `o` records. It also emits the
deprecated `maintainers` degradation tag containing only current accepted `M`
and `m` subjects. Role-aware clients ignore that fallback; older clients retain
the best representation available to them. Other authors' announcements remain
unchanged until those authors publish their own role or history mutation.

The `maintainers` fallback applies only when an announcement contains no
indexed `M`, `m`, or `o`. During partial migration, indexed edges from other
members can still vote while the selected announcement is legacy. Indexed
`m` without `M` in the selected announcement is the explicit no-lead boundary.

### Moderators

An active `o` from a confirmed maintainer assigns a moderator invitation. The
recipient acknowledges it with an active self-`o` and a relationship to a
confirmed member. Moderator confirmation uses a reciprocal fixpoint analogous
to maintainers, but moderator edges never extend maintainer authority.

Confirmed moderators may author status, label, subject, and cover-note events.
They cannot publish kind `30618` state, push protected branches, or merge. A
moderator cannot assign third-party roles.

The first membership API need not expose moderator assignment. Every
maintainer mutation preserves existing `o` role and history records, and
`repo leave` can end an acknowledged self-moderator role.

### Coordinates and repository data

A coordinate is `(kind, pubkey, identifier)`. Its pubkey is both the discovery
anchor and the only signing identity that can replace that coordinate's
announcement. This permanent coordinate control is not ownership of a central
repository roster. Local resolution priority remains:

1. explicit `--repo <REMOTE|NADDR|NOSTR-URL>`;
2. `nostr.repo`;
3. the current branch's tracked `nostr://` remote;
4. `origin` when it is `nostr://`;
5. the sole remaining distinct `nostr://` coordinate; and
6. `maintainers.yaml` only when no Nostr coordinate is configured.

Shared metadata follows the repository's existing authoritative recency rules.
Personal clone servers, relays, Blossom servers, and grasp preferences remain
authored independently and are unioned only from the confirmed component.

`maintainers.yaml` is a legacy coordinate fallback, not a role or history
store. `repo accept` does not re-root a checkout. `repo follow-lead` is the
explicit verified way to select the terminal announcement reached by the
current selected coordinate's lead-pointer path.

### Edge cases and failure rules

#### A stale acknowledgement reinstates a maintainer

If Alice ends and later restarts her real edge to Bob while Bob's reciprocal
consent remains active, Bob is confirmed again immediately. Historical copies
are irrelevant to this result. Bob uses `repo leave` to withdraw his own
consent.

#### A removal partitions the graph

`--remove-maintainer Bob` fails if Bob remains confirmed or if anyone besides
Bob loses authority. The error shows the retaining or cascading paths. Callers
then add a desired direct edge or remove each intended person explicitly.

#### Two same-identifier repositories meet

Neither add nor accept is a repository-merge command. They fail before an edge
joins components with distinct membership, history, or state. A future
reconciliation design must cover Git history, earliest unique commits, `u`
fork relationships, ref conflicts, membership histories, local coordinates,
and recovery before exposing an explicit merge action.

#### A history-unaware client replaces an announcement

The latest event controls the author's current statement. Its omission of a
role-history record does not erase copies retained by other confirmed
maintainers, including inactive records ending in `open`. The preferred
history resolver falls through to those copies and reports disagreement or
uncertainty.

A client may also omit or rewrite the author's current role intervals. That can
change the current graph because no protocol can force another signer to
preserve data. It cannot turn another author's historical copy into an active
edge.

#### A transition is never acknowledged

If no maintainer observes Bob's acceptance before his evidence disappears, the
exact start is unknowable. ngit reports an unknown boundary. Immediate command
guidance reduces this window but cannot eliminate decentralized relay loss or
hostile clients.

#### Concurrent membership edits

A command rechecks the publisher's latest event and affected graph before
signing. If either changed after preview, it aborts and asks the user to rerun
the intent. Addressable-event last-write-wins must not discard a concurrent
membership action silently.

#### Relay disagreement

If the client cannot establish a sufficiently complete announcement and state
set to decide whether a write joins or partitions a repository, it fails
closed. Reads may show partial information; membership writes require more
complete evidence.

#### The lead key is unavailable

Confirmed co-maintainers retain equal state and merge authority. They can
establish a replacement lead, subject to graph, history, and state checks. A
maintainer who still controls a coordinate that points to the unavailable lead
can redirect that coordinate to the replacement. Replicated histories let the
replacement adopt the retained view without depending on one lead event, while
conflicting copies remain visible.

Nobody can redirect the unavailable key's own coordinate. A checkout that
already selected that coordinate cannot be auto-forwarded by somebody else's
announcement; its user must verify and select another confirmed maintainer's
coordinate explicitly. This is also the recovery rule when an organization
pubkey's secret is lost.

#### A coordinate signer changes or withdraws a forward

Alice can replace her pointer to Bob with a self-pointer or a pointer to Carol.
Clients rooted at `alice/my-repo` use Alice's latest valid instruction and stop
recommending Bob. Checkouts that already followed Bob remain rooted at
`bob/my-repo` and are unaffected. If Alice's new path is pending or conflicting,
clients stay on Alice's coordinate and report the failure instead of guessing a
destination.

#### An organization pubkey changes operators

Clients cannot distinguish a signature made by the intended new operator from
one made by somebody who retained the same organization secret. They must not
describe changing operators as revoking the former operator or transferring
exclusive ownership. Moving users to a newly generated lead coordinate is the
only repository-level transition that stops later changes to the old
coordinate from affecting those users.

### Authorization summary

| Actor | Repository state | Merge | Status/labels/subject/cover note |
| --- | --- | --- | --- |
| Resolved lead | yes | yes | yes |
| Confirmed co-maintainer | yes | yes | yes |
| Confirmed moderator | no | no | yes |
| Invitee | no | no | no |
| Outsider | no | no | no |

Issue and proposal authors retain author-specific NIP-34 actions. The table
covers authority derived from repository roles.

### Required client invariants

The implementation and tests must make these statements true:

1. A fresh one-person repository emits no role or `maintainers` tag.
2. A sole maintainer's first add automatically materializes them as lead. An
   explicitly leadless add or remove, and a membership mutation in a
   multi-maintainer legacy view without an explicit lead, requires exactly one
   of `--lead-maintainer` or `--no-lead-maintainer`.
3. The normal first invitation materializes the inviter as `M` and invitee as
   `m`; supplying `--no-lead-maintainer` emits only `m`. A pending or
   conflicting lead path blocks instead of treating the flag as an override.
4. Acceptance records the inviter as `M`, the invitee as self-`m`, and cannot
   add unrelated people or self-promote.
5. Discovering acceptance promptly reports the separate
   `--acknowledge-maintainer-change` action to every confirmed maintainer whose
   history lacks it, without prompting; JSON returns the same action as
   structured data.
6. Every confirmed maintainer can replicate effective start and end intervals
   in `M`, `m`, or `o`. Departure timing prefers an explicit signed role end,
   then a signed deletion request, then a clearly labelled observation
   estimate; an `open` end sentinel keeps a copied interval historical-only.
7. The selected maintainer's history wins and omissions continue through the
   sibling NIP-34 distance and pubkey precedence.
8. `M` and `m` have identical maintainer authority; reciprocity defines the
   confirmed component. Lead resolution, including explicit no-lead, affects
   only forwarding, coordinate recommendations, and display.
9. Lead resolution starts at the selected coordinate, follows one active `M`
   per announcement, and terminates only at a confirmed self-`M`. Different
   targets outside that path do not create a global conflict.
10. Legacy listing-vote inference remains while the selected announcement is
    legacy; selected indexed `m` without `M` expresses the no-lead choice. A
    membership mutation migrates the selected event to indexed roles while
    retaining the accepted `M`/`m` degradation `maintainers` fallback.
11. Every mutation preserves unrelated relationships, role intervals,
    replicated history, metadata, and unknown tags.
12. Removing one maintainer fails if that person remains confirmed or the
    graph loses anyone else.
13. Declaring a lead never removes a confirmed maintainer or invitation; a
    violating path emits the required named add-first/remove-first error.
14. Force cannot combine lead declaration with removal or turn add/accept into
    a repository merge.
15. Add resolves the named pubkey's existing component, history, and state
    before publishing an edge.
16. Accept compares the invitee's existing announcement, earliest unique
    commit, `u` relationships, history, component, and refs.
17. An unexpected component join or conflict blocks before signing and reports
    which state would otherwise win.
18. A lead transfer never rewrites local coordinates automatically.
    Human-facing commands repeatedly offer `repo follow-lead` after verifying
    graph and state equivalence, until the user follows or the pointer changes.
19. A coordinate remains controllable by every holder of its signing key;
    changing its lead cannot transfer or revoke that control.
20. Metadata-only edits do not migrate legacy membership.
21. A historical copy can never authorize its subject, even when its final
    interval is `open`.
22. Current authorization remains defined when exact history is missing or
    disputed.

Each normal workflow and destructive edge case needs a unit-level graph,
history, and state fixture plus an integration test for the published
announcement, selected coordinate, and resulting authorization. Tests wait on
observable relay or Git state with bounded deadlines and never use fixed
sleeps.
