# Maintainer Model

> **Status:** this is the desired final maintainer model and public API. The
> ngit client does not yet provide every protection and edge-case workflow
> described here; remaining client work is tracked in
> [follow-up actions](maintainer-model-follow-up-actions.md). The two preceding
> revisions of this file record the pre-role and initial indexed-role models.
> The ngit-ci coordinator implements the coordinator-relevant current
> read-side model: validated indexed roles, explicit lead forwarding, legacy
> and leadless reciprocity, former-coordinate redirects, and fail-closed
> repository state, controls, relay confidence, cache, and secret authority.

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
- **Co-maintainer:** may publish Git state, create or push merge commits, and
  moderate issues and proposals. They may also assign additional maintainers;
  each assignment requires the recipient's matching acceptance before they are
  confirmed.
- **Lead maintainer:** has the same protocol permissions as a co-maintainer and
  is additionally responsible for managing the maintainer and moderator roster.
  When a lead is present, lead-aware tooling normally restricts roster
  management to them and expects each co-maintainer's coordinate to forward to
  the lead.
- **Invitee:** has no maintainer authority until they accept. Joining always
  requires statements from both sides.
- **Moderator:** may manage issues and proposals, including publishing a
  `merge` status for a merge commit already present in the repository. That
  authority does not extend to publishing kind `30618` repository state or
  creating or pushing the merge itself.

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

`ngit repo accept` accepts only the offered co-maintainer role. It does not make
Bob lead, add another person, or replace Alice's relationships. Once his
announcement is published, he has the same repository authority as Alice.

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

Bob's acceptance already records his active self-role and lead relationship.
Alice, as lead, runs the acknowledgement command above to correct her active
assignment's start. Every other co-maintainer whose announcement lacks Bob's
transition is instead prompted to run:

```bash
ngit repo follow-lead
```

For a maintainer this command is also an idempotent history sync: it keeps their
own role and direct lead relationship active, while copying Bob's effective
interval as historical-only. The same rule applies when a maintainer is removed
or leaves. History replication is not a lead-only job.

#### 6. The repository continues normally

Alice or Bob can publish state, merge, and perform ordinary maintainer actions.
In a lead-shaped repository, ngit directs membership changes through the
resolved lead: a co-maintainer asks the lead to invite or remove somebody. In a
deliberately leadless repository, any confirmed maintainer may make the change
with the required `--no-lead-maintainer` choice. All adds remain subject to the
repository-join checks described below.

### Everyday commands

| Intent | Command |
| --- | --- |
| Invite one maintainer | `ngit repo edit --add-maintainer <npub>` |
| Remove one relationship | `ngit repo edit --remove-maintainer <npub>` |
| Choose a lead | `ngit repo edit --lead-maintainer <npub>` |
| Make a leadless membership change | add/remove plus `--no-lead-maintainer` |
| Accept an invitation | `ngit repo accept` |
| Record a transition in the lead roster | `ngit repo edit --acknowledge-maintainer-change <npub>` |
| Leave a repository | `ngit repo leave` |
| Sync history and follow the resolved lead | `ngit repo follow-lead` |

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

`--force` has one narrow add/accept meaning: after a state-only collision is
reported, it keeps the complete ref state of the checkout running the command
and replaces the other view. It is not needed in the normal workflow and never
overrides additional membership, identity, history, or object-availability
errors.

### Adding a maintainer

```bash
ngit repo edit --add-maintainer <carol-npub>
```

If a lead is already explicit, it is preserved. Carol is invited until her own
announcement acknowledges the repository with an active self-role and active
lead relationship. When she accepts, the lead is shown the acknowledgement
command and other co-maintainers are shown `ngit repo follow-lead` as they next
use ngit.

In a lead-shaped repository, ngit accepts this command only from the resolved
lead. A co-maintainer receives an actionable error instead of publishing a
third-party edge:

```text
only the resolved lead should add maintainers to this repository
ask <alice-npub> to run:
  ngit repo edit --add-maintainer <carol-npub>
```

Clients must still interpret active third-party relationships authored by
other software; the recovery is specified under “Edge cases and failure
rules.”

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

This includes a co-maintainer's deliberate assignment. If Bob actively lists
Carol, Alice cannot remove Carol while retaining Bob: Bob's edge keeps Carol in
the reciprocal graph. The safe repair is for Alice to add Carol if necessary,
then ask Bob to run `ngit repo follow-lead`. Once Bob's edge is historical-only,
Alice can keep Carol or remove her with a separate ordinary command. The tool
does not disguise this sequence as a successful removal or generic force
override.

Once the removal is observed, other co-maintainers are shown `ngit repo
follow-lead` to record Bob's end time. Their deferred history copies never delay
the current removal. Bob sees a repository-health error explaining that the lead
no longer assigns him and directing him to the same command; his maintainer
operations remain blocked until his announcement ends his self-role.

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
`repo follow-lead` verifies whether it is safe to converge the user's
announcement, when they are a maintainer, and local coordinate.

### Leaving a repository

```bash
ngit repo leave
```

Leaving publishes an ended self-role while retaining an active `M` redirect to
the lead. Because a maintainer's self-role takes precedence, Alice continuing
to list Bob does not make Bob active after Bob leaves. Bob returns only by
publishing a later self-role start that acknowledges the repository again. If
leaving would also disconnect other maintainers, ngit refuses and explains
which relationships must be dealt with first. ngit does not expose an option
to end the redirect at the same time.

Bob's signed self-role end is the preferred history boundary. If departure is
instead expressed by a signed deletion request for his announcement, its
`created_at` is the best available end time. If a departure is established but
neither signed boundary is available, a maintainer records the earliest
well-supported observation of Bob's disappearance as an estimated end. Clients
label that boundary as estimated and replace it if stronger signed evidence is
later found. Failure to fetch from one relay is not departure evidence.

### Transferring the lead

The target first joins as an ordinary confirmed maintainer and retains the
complete resolved history. Bob then prepares to lead:

```bash
ngit repo edit --lead-maintainer <bob-npub>
```

Because Bob is naming himself, ngit publishes Bob as lead with the complete
roster. Alice remains the resolved lead for coordinates that still start from
her; Bob is only ready to receive the handoff.

Alice can now point her coordinate to Bob:

```bash
ngit repo edit --lead-maintainer <bob-npub>
```

Alice becomes a co-maintainer. Her replacement retains the full history, keeps
only her self-`m` and direct `M:Bob` relationship active, and changes every
other current interval to historical-only. The chain is now Alice → Bob → Bob,
so Bob is the resolved lead.

Alice and every other co-maintainer are then repeatedly shown:

```bash
ngit repo follow-lead
```

For a confirmed maintainer, that command first republishes their complete
historical view with an active self-`m` and Bob, rather than Alice, as their
active direct `M`. Every current third-party role copied from the resolved
history uses `defer`, so it should not retain a person whom Bob removes. An
unexpected active third-party `m` is handled by the guarded repair below. The
command then switches the local `nostr://` coordinate and `nostr.repo` after
verifying that Bob's rooted view contains the same membership and Git state. A
non-maintainer runs the same command but changes only local configuration
unless their own stale announcement must first record a removal as described
below.

Until every maintainer follows, an old pointer such as Carol → Alice → Bob can
still resolve the repository, but clients show Carol's convergence action as
pending. A completed handover has every co-maintainer pointing directly to Bob.
Bob cannot remove Alice while another maintainer still depends on Alice's
pointer; ordinary removal preflight reports the resulting disconnection.

If Bob has not first published every confirmed co-maintainer and outstanding
invitation in his active roster, Alice's command fails and names each missing
person. It tells Alice to ask Bob to list them before retrying, or to remove
each person explicitly before transferring the lead. There is no force
shortcut.

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

Although lead-aware tooling treats the lead as roster coordinator, the wire
declarations are not cumulative. A lead's active self-`M` both confirms their
maintainer role and declares them as lead; they do not also need an active
self-`m`. Ngit's canonical co-maintainer output instead acknowledges their role
with an active self-`m` and points to the lead with an active `M`; protocol
readers resolve authority from the reciprocal active-edge rules below.

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
   role record, but an explicit `defer` sentinel retains an interval as
   historical-only without making a current assignment through that record.
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
- A **confirmed maintainer** is named by a valid, active `M` or `m` listing
  from a confirmed maintainer in the selected component and has a valid,
  active `M` or `m` listing back to a confirmed maintainer. In the normal
  topology the resolved lead's roster seeds this reciprocal fixpoint.
- An **invited maintainer** is listed by the discovered graph but has not made
  the acknowledgement needed to join it.
- The **maintainer graph** contains valid, active `M` and `m` assignment edges.
  A signed self-role end vetoes the author's maintainership even while another
  member still lists them. Historical-only records do not add current edges.
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
- A literal `defer` in an end position retains an interval as historical-only
  without asserting a numeric end. Its even-length tag is inactive for
  authorization or lead forwarding.
- A pubkey may have one record for each role letter. A role transition closes
  the old letter and starts the new one rather than rewriting the past.

Examples:

```text
["m", "<pubkey>"]                           # active, start unknown
["m", "<pubkey>", "100"]                  # active since 100
["m", "<pubkey>", "100", "200"]         # ended at 200
["m", "<pubkey>", "100", "200", "300"] # active again since 300
["m", "<pubkey>", "300", "defer"]        # historical copy, not active
```

The author is part of the fact. Alice's `m:Bob` is Alice's outgoing
relationship to Bob. Bob's `m:Bob` is Bob's acknowledgement of his own role.
The same subject in two events is not one shared record.

Active `M` and `m` records have identical maintainer authorization weight. In
the normal lead-shaped topology, ngit publishes an active self-`M` and the
recommended active roster for the lead. Its canonical co-maintainer acceptance
shape is an `M` naming the lead and an `m` naming the signer. That is a
recommended publishing shape, not an additional read-side condition for
reciprocal authority.

A co-maintainer also publishes the complete resolved `M`, `m`, and `o` history.
Every third-party interval copied solely as history, rather than as this
author's assignment, ends in `defer` unless the author records a numeric end.
Their active self-`m` records ngit's canonical signed acceptance of their
assigned role, and their active `M` identifies and reciprocates with the lead.
A record ending in `defer` is historical-only and contributes no current graph
edge. It cannot satisfy reciprocity, whether it is a self-role, lead pointer,
or third-party listing.

The `defer` convention is a statement of intent, not a protocol restriction. A
third-party client may publish an active `m` from a co-maintainer to somebody
else. That record is a real assignment or invitation, participates in
reciprocal graph resolution, and appears in `maintainers`; clients must not
silently reinterpret it as `defer`. ngit does not create this shape in a
lead-shaped repository and treats it as the recoverable edge case specified
below.

The deliberately leadless topology is different: because no lead publishes an
active roster, confirmed co-maintainers use active reciprocal `m` assignments.
Those edges have normal authorization and repository-join consequences.

The sibling NIP-34 draft should retain its recommendation that a co-maintainer
actively list themselves and the lead. It must add the `defer` extension for
records copied for other maintainers and moderators when the author makes no
current assignment through them. Those copies retain history without becoming
assignments.

If an announcement has no `M`, `m`, `o`, or legacy `maintainers` tag, its
author is the implicit sole maintainer. This is the preferred one-person wire
format.

If any `M`, `m`, or `o` tag is present, indexed roles are authoritative and the
deprecated `maintainers` tag is only a compatibility projection. Otherwise
`maintainers` supplies legacy listings.

A role-aware announcement always emits exactly one `maintainers` tag containing
the subjects of every active `M` and `m` record, and no others. This is exact set
equality: it includes active invitations as well as confirmed assignments, but
excludes numeric-ended records, records ending in `defer`, moderators, and
duplicates. An empty projection is emitted as `["maintainers"]`. The legacy tag
cannot preserve the distinction between an invitation and a confirmed role;
older clients receive the active assignment roster as the least misleading
degradation available.

If the projection disagrees with the active `M` and `m` records, the indexed
records win. The mismatch is nevertheless a repository-health error in the
author's announcement. ngit requires the author to repair it before another
repository edit, as described under “Edge cases and failure rules.”

The happy-path announcements progress like this. Alice's first invitation at
`T1` automatically establishes Alice as lead and creates Bob's pending edge:

```text
["M", "<alice-pubkey>", "T1"]
["m", "<bob-pubkey>", "T1"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>"]
```

Bob's acceptance at `T2` actively acknowledges his own role and Alice's lead:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>"]
```

When Alice acknowledges the acceptance, her replacement changes Bob's first
start from `T1` to the effective start `T2`. Her active roster and degradation
projection remain Alice and Bob, and her active `M:Alice` record is otherwise
unchanged.

### Replicated role history

The role tags in the sibling NIP-34 draft already carry start/end history and
define precedence between conflicting copies. Effective membership history is
therefore replicated in `M`, `m`, and `o`; it does not need another tag type.

The missing distinction is how a maintainer can retain an interval without
currently assigning its subject or asserting that the interval ended. This
proposal reserves the literal `defer` in an end position:

```text
["m", "<bob-pubkey>", "200", "defer"]
```

This says: “my historical view has Bob confirmed from time 200, but I neither
assign him now nor assert a numeric end.” Other active role records may or may
not assign Bob; current status is resolved from the active maintainer graph,
normally rooted at the resolved lead. The record has an even number of elements,
so current ngit and clients following the existing parity rule treat it as
inactive. A numeric end replaces `defer` when the author records one:

```text
["m", "<bob-pubkey>", "200", "300"]
```

The sibling NIP-34 draft must be clarified so a valid history boundary may be
this literal sentinel as well as a Unix timestamp. Clients must never parse
`defer` as an end time or as an active role assignment.

The lead uses an ordinary omitted end for real current assignments. A current
co-maintainer does the same for their lead `M` and self-`m`, because those two
active records are their reciprocal acceptance. They use `defer` only for
intervals belonging to other people whose current assignment they defer to the
active graph. If a co-maintainer becomes lead, they close their former lead `M`
and self-`m`, start an active self-`M`, and publish the full active roster. A
former lead becoming a co-maintainer keeps an active `M` to the new lead and
active self-`m`, while changing relationships covered by the prepared lead into
historical-only copies.

Role-aware publishers preserve the resolved histories they know. When the lead
observes a confirmation whose effective start is not recorded in its active
assignment, every ngit command shows
`--acknowledge-maintainer-change <npub>`. When a co-maintainer observes a
missing start or end, commands show `ngit repo follow-lead`; this idempotently
synchronizes replicated history without changing their active self-role or
resolved lead. If the announcement unexpectedly contains an active third-party
assignment, the command enters the guarded repair workflow instead of treating
it as ordinary history. Neither history action silently adds, removes, or
accepts a third-party relationship.

An invitation is not effective maintainership. Alice initially publishes
`m:Bob` with `T1` so the invitation is discoverable. Once Bob accepts at `T2`,
Alice changes the beginning of that first interval to `T2`: she is now signing
her observation that Bob became a maintainer then, rather than claiming he was
one from the invitation time. Other maintainers without a real Bob edge retain
the same effective start using the inactive `defer` form:

```text
T1  Alice publishes m:Bob,T1             Bob is invited
T2  Bob publishes M:Alice,T2 + m:Bob,T2  Bob becomes confirmed
T3  Alice records m:Bob,T2                accepted start retained
T4  Carol follows the lead               copies m:Bob,T2,defer
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
When a checkout still selects a co-maintainer, that co-maintainer's current
active `M` identifies the lead, so the lead's fuller record is normally the
nearest fallback.

An omitted record does not erase history retained elsewhere. An explicit
replacement interval corrects the preferred view; omission falls through to a
retained copy.

This precedence applies only to historical display and past-event filtering.
Current authority in a lead-shaped repository is the reciprocal fixpoint seeded
by the lead's active roster. An externally authored active `m` from a confirmed
co-maintainer can extend that fixpoint when its subject publishes any valid,
active `M` or `m` listing back to a confirmed maintainer. The reciprocal edge
does not need to use a particular letter or point directly to the terminal
lead. A deliberately leadless repository uses its active reciprocal graph
without that seed. A `defer` historical copy is always inactive for
authorization and routing, regardless of which author published it.

When a lead removes Bob in the ordinary lead-shaped graph, the lead closes its
edge and effective interval at the removal time. Bob leaves immediately only
when no confirmed co-maintainer deliberately assigns him. Other maintainers may
still carry `defer` historical copies until they acknowledge the removal; those
copies cannot delay it. A real active alternative edge can retain Bob, so
ngit's removal command fails before publication and invokes the guarded
lead-cover-then-follow recovery instead of silently removing either signer.

A leadership transfer cannot complete while the proposed lead's history is
missing resolved transitions or their active roster omits a confirmed member
or outstanding invitation. The new lead first acknowledges the retained view
and publishes the complete active roster. The old lead may then become an
active co-maintainer of the new lead, after which every other co-maintainer
updates their direct active `M` with `repo follow-lead`. Repositories with
conflicting histories require the same explicit reconciliation workflow as
conflicting Git state.

### Reciprocal graph resolution

Clients select the latest addressable announcement for each author using
NIP-01 ordering, then distinguish active assignment from signed reciprocal
acknowledgement.

For the normal lead-shaped topology:

1. Starting from the selected coordinate, follow its active `M` view until an
   active self-`M` identifies the lead.
2. Seed the candidate roster with the lead's active `M`, `m`, and `o` records.
3. A candidate maintainer is confirmed when a confirmed maintainer's valid,
   active `M` or `m` names them and their latest announcement contains a valid,
   active `M` or `m` naming a confirmed maintainer. The reciprocal listing need
   not use a particular role letter or point directly to the lead.
4. Add every subject of a confirmed maintainer's active third-party `M` or `m`
   to the candidate roster and repeat confirmation to a fixpoint. Records
   ending in `defer` never enter this step.
5. A candidate whom a confirmed maintainer lists without that signed
   reciprocal listing remains invited. A numeric self-role end is an explicit
   departure and takes precedence over every assignment.
6. Closing an assignment removes the candidate as soon as no confirmed
   maintainer actively lists them. The candidate is expected to acknowledge
   that removal by ending their active self-role. Until they do, their valid,
   active reciprocal listing remains standing pre-acceptance: restarting the
   assignment confirms them immediately without a new candidate event. If the
   candidate has ended their self-role, the restarted assignment is instead a
   new invitation and they must append a new active start to accept it.

This remains reciprocal: a confirmed maintainer assigns the role and the
candidate signs an active acknowledgement bound to that repository. A
co-maintainer's active lead `M` and self-`m` are ngit's canonical way to record
that relationship, but the resolver accepts any valid active reciprocal
`M`/`m` edges. An active third-party `m` may extend the roster and import its
subject's state after reciprocity even though ngit flags that shape for
convergence; a `defer` copy cannot.

Only syntactically valid, currently active `M` records create lead pointers. An
`M` with malformed role history is ignored for authority and lead resolution
and reported as repository health information; its mere presence does not turn
the absence of a valid active `M` into an incomplete explicit path. When no
valid active `M` remains at the selected coordinate, resolution uses the
selected-rooted leadless graph. Once resolution follows a valid active `M`, a
missing announcement, multiple active targets, or a cycle fails closed and
seeds no authority.

The deliberately leadless topology has no active lead roster, so it retains
the reciprocal active-`m` fixpoint rooted at the selected maintainer. A cycle
of unconfirmed invitees cannot bootstrap itself into authority. This extra
topological complexity is one reason leadless governance is an advanced mode.

If Bob already acknowledges a different same-identifier lead or leadless
component, adding or accepting him can still join repositories. ngit detects
that existing acknowledgement during preflight and refuses the implicit join.
One active lead view per maintainer and identifier prevents Bob from silently
belonging to two lead-shaped virtual repositories at once.

Current kind `30618` state and maintainer-only NIP-34 actions are filtered
through the confirmed component. Starting from another coordinate can expose a
different component while the graph is partitioned.

### Lead resolution

Lead resolution is a semantic forwarding walk rooted at the selected
coordinate. It follows each announcement's one active `M`. A lead points to
themselves; a co-maintainer or former maintainer preserving a redirect points
to the lead. If the target points onward, resolution continues. A confirmed
maintainer whose active `M` points to themselves is the terminal lead. An `M`
ending in `defer` is historical-only and cannot participate in this walk.

```text
Carol → Alice → Bob → Bob
                       ^ terminal lead
```

An old co-maintainer pointer can traverse a handover temporarily, but clients
require its signer to converge directly on the new lead with
`repo follow-lead`. This also preserves coordinate ownership: replacing
Alice's pointer immediately changes where `alice/my-repo` leads, but cannot
change a checkout that already selects Bob.

Resolution has seven results:

1. **Implicit sole.** A one-person component with no role or legacy tags treats
   its author as workflow lead without emitting `M`.
2. **Explicit.** The selected pointer chain follows active `M` views and reaches
   a confirmed maintainer with an active self-`M`. That pubkey is lead.
3. **Legacy inferred.** With no active `M` view at the selected announcement
   and a selected announcement that has not adopted indexed roles, every
   active legacy listing and every indexed active `M` view between two
   distinct confirmed maintainers is one vote for its subject. The unique
   highest positive count is inferred as lead.
4. **Explicit none.** The selected maintainer published indexed `m` roles
   without `M` through the no-lead workflow. Legacy inference is disabled for
   that rooted view.
5. **None.** A legacy graph has no positive unique vote winner.
6. **Pending.** A pointer target has not accepted, its announcement is missing,
   or a handover has not yet established the target's complete active roster.
7. **Conflict.** An announcement supplies multiple active lead views, a
   target has explicitly left, the path cycles, or the forwarding walk
   otherwise cannot produce one safe continuation.

Legacy vote inference remains while the selected announcement is legacy.
Listings from legacy members and active `M` views still contribute to its vote
count, so one member migrating does not erase an established lead. Historical
`M` records ending in `defer` do not vote. Once the selected maintainer
deliberately publishes indexed `m` without `M`, inference stops for that rooted
view. This makes `--no-lead-maintainer` expressible while preserving
repositories that have not opted into the new role model.

The Alice/Bob happy path begins Bob → Alice → Alice, with both pointers active.
Bob prepares an active Bob → Bob roster before Alice changes to an active
Alice → Bob pointer plus active self-`m`. Other co-maintainers may temporarily
resolve Carol → Alice → Bob, but each is expected to republish Carol → Bob
directly. A tied legacy co-maintainer topology remains leadless and continues
to authorize all reciprocal members.

Active `M` views outside the selected pointer path do not create a global
conflict merely because they name another lead. They affect views rooted at
coordinates that reach them. Numeric-ended and `defer` historical `M` intervals
never enter the walk.

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
- When a confirmed maintainer's active `M` reaches the lead indirectly, repeat
  the same guidance until they publish a direct pointer.
- Never rewrite an announcement or local coordinate automatically. For a
  confirmed maintainer, an explicit follow first preserves the full history
  while keeping an active self-`m`, changing the active `M` pointer, and making
  third-party copies historical-only. An unexpected active third-party
  assignment invokes the safe convergence checks below. The command then
  anchors the checkout at the lead coordinate. For a removed maintainer it ends
  the self-role while retaining that pointer. For users who never held a role
  it changes only local configuration.
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
- In a lead-shaped repository, reject `--add-maintainer` and
  `--remove-maintainer` from co-maintainers and identify the resolved lead who
  should perform the action.
- Present a unilateral listing as an invitation. Do not call the invitee a
  maintainer or accept their state until reciprocity confirms them.
- Report a newly confirmed start or end as soon as it is observed. Show the
  lead the exact `--acknowledge-maintainer-change` command and show
  co-maintainers `ngit repo follow-lead` to synchronize their replicated
  history. Do not open an interactive prompt or publish either change
  automatically.
- If a signed-in maintainer's active self-role is no longer assigned by the
  resolved lead, reject maintainer operations and report the removal after
  every ngit or Git command in the checkout. Allow `ngit repo follow-lead` to
  end the self-role while retaining the lead redirect and copied history.
- Resolve externally authored active third-party `m` edges from co-maintainers,
  but report them to both co-maintainer and lead after every ngit or Git command.
  Gate `repo edit` until the lead covers every subject and the co-maintainer
  runs the safe `repo follow-lead` repair.
- When the logged-in signer authored a role-aware announcement whose
  `maintainers` projection differs from its active `M`/`m` roster, use the
  indexed roles, warn after every ngit or Git command in that checkout, and
  offer only `ngit repo edit --fix-maintainers` as the immediate repair. Report
  the same repository-health error as structured data in JSON output.
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

A confirmed maintainer who already selected Bob but still points through Alice
instead sees:

```text
your maintainer announcement still reaches bob/my-repo through alice/my-repo
point directly to the lead with: ngit repo follow-lead
```

These forwarding warnings are advisory: the requested command continues, and
Alice's coordinate remains a valid user-selected trust root. They stop only
after the required local and, for a maintainer, announcement changes are
complete, if Alice withdraws the pointer, or if the pointer no longer resolves
safely.

A maintainer removed by the lead sees:

```text
error: <alice-npub> no longer lists you as a maintainer of this repository
update your announcement and retain its history with: ngit repo follow-lead
```

The recovery republishes an ended self-`m` and leaves the active `M` redirect in
place. It does not regain maintainer authority.

### Required safety posture

Without the narrow state-replacement override below, membership operations
fail before publication when they would:

- remove or add people beyond the named action;
- withdraw an invitation as an accidental side effect;
- connect a same-identifier component containing anybody beyond the named
  invitee;
- select a different earliest unique commit or fork relationship;
- make a different kind `30618` state authoritative, including state authored
  before its signer was invited into this component;
- make a previously ignored maintainer or moderator action authoritative; or
- make branches or tags appear, disappear, or change OID unexpectedly.

Errors name the affected people, coordinates, and refs. A repository merge or
several removals are separate decisions, not meanings assigned to `--force`.
For add and accept only, `--force` may override a conflict limited to kind
`30618` refs by choosing the command runner's current repository state. It does
not override identity, graph, role-history, or event-authorization failures,
nor a failure to make every chosen OID fetchable.

### Membership mutation contract

Kind `30617` is replaceable and contains a complete event body, but the API
applies one relationship delta plus, when required, one explicit governance
choice. Before signing, every membership mutation must:

1. fetch the latest announcements and state reachable from the selected
   component, every named pubkey, and every component reachable from those
   pubkeys;
2. preserve unrelated roles, relationship intervals, replicated history,
   metadata, unknown tags, and personal infrastructure;
3. construct the proposed replacement event in memory;
4. resolve confirmed maintainers and moderators before and after the change;
5. resolve repository identity and state, including the `r` earliest unique
   commit, informational `u` fork links, every candidate kind `30618` event,
   every Git ref/OID, and the complete local ref map of the checkout running
   the command;
6. display the intended and consequential changes, including the exact local
   ref changes that would align conflicting state and the inverse ref changes
   that a forced replacement would impose;
7. recheck the event IDs used for every affected announcement and state view,
   and fail if any fetched predecessor or candidate changes before
   publication; and
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
- every kind `30618` event that becomes eligible or ceases to be eligible,
  which event would supply the resolved state, and refs that would appear,
  disappear, or change OID;
- newly authorized maintainer or moderator events whose effect remains
  current;
- state OIDs that cannot be fetched from the post-change component's
  advertised Git servers; and
- selected-coordinate changes.

For a state mismatch, human output classifies each difference as a branch or
tag to add, update, or remove in the repository controlled by the command
runner. It reports a default-branch change separately. JSON exposes the same
two directional comparisons as structured ref actions: align the runner's
repository with the other component, or replace the other component with the
runner's state. An annotated tag and its peeled `^{}` companion are reported as
one tag action rather than as two changes the user must decipher.

Ordinary success requires that graph effects match the command's name and no
conflicting state is selected. Human errors and JSON output expose exact npubs,
refs, and coordinates.

### Confirmation can activate pre-existing state

Confirmation changes authorization, not only the role display. Bob may have
published a kind `30618` event while his same-identifier coordinate belonged to
a different virtual repository. When an add or acceptance makes Bob confirmed,
that existing event can immediately become eligible for state resolution even
though the membership command publishes no new kind `30618` event. Its age does
not by itself make the transition safe.

The invitation event is not a state lock. Preflight uses the latest eligible
announcement and state events at the confirmation boundary, including anything
published after the invitation.

The collision works in both directions. Bob's event could replace Alice's
resolved refs, or Alice's component could replace the state Bob previously saw
through his coordinate. A client must compare the pre-change view from each
component with the simulated post-change view. It must not describe one side as
the repository and silently discard the other merely because that side wins
the normal event-ordering rule.

Without `--force`, state is compatible only when the post-change resolution
preserves the same repository identity, earliest unique commit, fork
relationship, default branch, and complete ref/OID map. Every resolved OID must
also remain fetchable. A ref addition, deletion, rename, or OID change blocks
the command. Equivalent state events may have different authors or event IDs;
that difference is safe only when their resolved repository data are otherwise
identical.

When state is incompatible, the user must choose and complete one of these
actions before retrying:

1. **Keep the repositories separate.** Publish the invitee's repository,
   state, and required Git objects under a new identifier, verify that the new
   coordinate works, and then retire or reconcile the invitee's old
   same-identifier relationships and state.
2. **Align the command runner's repository.** Apply the reported branch, tag,
   and default-branch changes to the checkout used for the add or acceptance,
   publish that aligned state, and retry. For add, the instructions change the
   inviter's repository to match the invitee's state. For accept, they change
   the invitee's repository to match the inviting state.
3. **Replace the other state.** Rerun the same add or acceptance with
   `--force`. This explicitly keeps the command runner's complete current ref
   map and makes it the post-confirmation repository state. The preview lists
   the branches and tags this will add, update, or remove from the other view.
4. **Merge the repositories deliberately.** Reconcile Git history and refs,
   repository identity, fork metadata, membership history, and every imported
   member before creating the reciprocal edge. Until a dedicated merge
   workflow exists, ngit refuses this choice rather than approximating it with
   add, accept, or the state-replacement meaning of `--force`.

For example, an accept whose only conflict is Git state fails with an error
like this:

```text
cannot accept this maintainer invitation: repository state differs

to align the repository you control before accepting:
  add branch     refs/heads/release at <alice-release-oid>
  update branch  refs/heads/main from <bob-oid> to <alice-oid>
  remove tag     refs/tags/experiment at <bob-tag-oid>
  set default branch from experiment to main

publish the aligned state, then run:
  ngit repo accept

or keep your current branches and tags and replace the joined state with:
  ngit repo accept --force

--force would update refs/heads/main to <bob-oid>, remove
refs/heads/release, add refs/tags/experiment at <bob-tag-oid>, and set the
default branch to experiment for the joined repository
```

The add error uses the same format but tells the inviter how to align the
repository they control and shows
`ngit repo edit --add-maintainer <npub> --force` as the override. The action
list is computed against the command runner's complete ref map; it never tells
them to mutate somebody else's checkout.

`--force` is valid only when the sole unresolved difference is kind `30618`
state. It cannot import an unexpected maintainer or moderator, discard an
invitation, choose between conflicting role histories, change `r` or `u`, or
refer to unavailable Git objects. Before publishing, ngit uploads every object
needed by the chosen refs, publishes a fresh state event signed by the command
runner that orders after every candidate used by the preview, and verifies that
the post-confirmation resolved state exactly matches the forced preview. If it
cannot establish that order safely, it fails. The flag is non-interactive and
the JSON result records that state replacement was explicitly forced.

Without `--force`, publishing reconciled state or a signed deletion request for
old state is a separate decision; `repo accept` must never do either
implicitly. If the invitee's old component has other confirmed members, the
invitee also cannot treat that component as disposable on everybody else's
behalf. They must first transfer, re-identify, or deliberately merge it with
those members' participation.

### Adding a maintainer can join repositories

`--add-maintainer Bob` is not always just an invitation. Bob may already have a
same-identifier announcement that actively acknowledges Alice. In that case
Alice's new edge completes reciprocity immediately: the add is also the
acceptance boundary, without Bob running `repo accept`. The command must run
the same state and component preflight as an explicit acceptance before Alice
publishes anything.

Bob may also list Tom, who may list Carol, with reciprocal relationships of
their own. The preflight walks that complete confirmed fixpoint rather than
checking Bob alone. Alice's new edge can otherwise import Tom and Carol, their
role histories, and every eligible kind `30618` event into Alice's view. It can
also make Alice's state displace the state previously resolved by all three.

Preflight distinguishes:

1. **Unacknowledged invitation.** Bob does not acknowledge Alice's component.
   Only an invitation is added.
2. **Expected confirmation.** Bob already acknowledges the same component, the
   acknowledgement is not tied to an earlier closed assignment interval, and
   no extra member, invitation, identity, history, event authorization, or
   state conflict enters. The preview reports that Bob becomes confirmed
   immediately rather than describing the operation as a pending invitation.
3. **State replacement required.** Bob would be the only newly confirmed
   person and repository identity and history agree, but the kind `30618` ref
   maps differ. The command fails with the two directional ref lists above;
   Alice may align her repository and retry or explicitly keep it with
   `--force`.
4. **Repository join.** The operation adds an unexpected confirmed pubkey,
   connects another component with other members, changes identity or fork
   metadata, introduces conflicting history, or newly authorizes role-scoped
   actions. It fails before publishing and `--force` cannot change that result.

For example:

```text
cannot add <bob-npub> safely

their existing announcement already acknowledges this repository, so this
add would confirm them immediately and change resolved repository state:
  refs/heads/main  <alice-oid>  would become <bob-oid>

their active relationships would also import:
  <tom-npub> via <bob-npub> -> <tom-npub>

ask <bob-npub> to preserve their repository under a new identifier or
reconcile its membership and state with this repository before retrying
--force is not available for a repository join
```

A join error shows both components, the connecting edges, history differences,
every transitively imported pubkey, and a ref-by-ref state comparison in both
directions. It suggests `--force` only when removing the state difference would
leave an otherwise ordinary confirmation of Bob alone. Operators with a real
component join use one of the separate, align, or merge choices above. Until
the required workflow exists, ngit conservatively refuses that join.

### Accepting with an existing repository

`repo accept` replaces the invitee's own kind `30617` event for that author and
identifier. It may also join reciprocal components. This is dangerous when Bob
already uses the identifier for an experimental fork with his own `u` upstream
tag, earliest unique commit, membership history, or kind `30618` state.

It is equally dangerous when Bob's current announcement has active
relationships to Tom or another same-identifier component. Acceptance must
preserve those signed relationships while simulating the result; it cannot
silently omit them or rewrite them to `defer` just to make Alice's invitation
safe. If they would join another person or repository, acceptance fails and
names every imported path.

Before acceptance, ngit resolves:

- the inviting component from the selected coordinate; and
- Bob's same-identifier component, announcement metadata, history view, and
  state before replacement.

It previews the post-acceptance graph and existing state-selection result. It
shows whether Bob's fork relationship, earliest unique commit, history, or
refs would be retained, replaced, or imported into the joined component. The
comparison includes Bob's latest state even when it predates the invitation
and includes every state reachable through Bob's active relationships.

When differing refs are the only conflict, ordinary acceptance ends with the
alignment error above and offers `ngit repo accept --force`. When repository
identity, fork metadata, history, or additional members also conflict, the
error explains why state replacement cannot make the acceptance safe:

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

preserve the fork under a new identifier or reconcile both repositories
before accepting
--force cannot override an earliest-unique-commit or fork-identity conflict
```

The reverse direction is reported when Bob's state would displace Alice's.
“Replace” means disappear from the resolved view; relay retention of the old
event is not a recovery guarantee.

Acceptance is safe when Bob has no same-identifier repository, is already in
the same component, or both components, identity metadata, histories, and
state are compatible and no unexpected person becomes confirmed. Cosmetic
metadata can follow the ordinary shared-field rules. Conflicting identity,
`u`, history, or authorization always blocks. Conflicting refs block unless
they are the only difference and Bob explicitly chooses the scoped `--force`
state replacement.

A successful acceptance, or an add that confirms Bob immediately, invalidates
state and membership caches derived from Bob's former component. Before Bob can
push or run any command that could publish kind `30618`, the client fetches the
accepted component and compares his local refs with its resolved state.
Divergent local work is preserved on a new identifier or incorporated through
an explicit target-repository change; it is never published merely because no
conflicting Bob-authored state event existed during confirmation.

### Lead declaration safety

`--lead-maintainer <npub>` changes the publisher's semantic lead statement. If
the publisher names themselves, they become a prepared lead by publishing an
active self-`M` and the complete active roster. If they name somebody else,
they become a co-maintainer: their active `M` names the proposed lead, their
active self-`m` accepts their own role, and every other current role uses
`defer` as replicated history.

This convention does not give the target protocol ownership of the roster,
but it changes which event actively assigns the roster. The target must already
be confirmed and must prepare first. The normal handoff is “add, accept,
acknowledge history, let the proposed lead publish the complete active roster,
then let the old lead point to them.”

ngit therefore simulates the exact post-declaration graph. If Bob's prepared
roster omits Carol and Alice is her only active assigner, reducing Alice's
active relationships to Bob and herself would remove Carol. The same rule
protects outstanding invitations. Co-maintainer `defer` histories cannot cover
the omission because they never authorize their subjects. An externally
authored active third-party edge can cover Carol, in which case the preview
reports that real path; it does not misclassify the edge as replicated history.
Bob must still absorb or explicitly reconcile it before claiming a complete
prepared roster.

That operation fails even if an earlier implementation offered `--force`:

```text
cannot transfer the lead to <bob-npub> because their active roster would
remove confirmed maintainers from the repository graph:
  <carol-npub>
  <dave-npub>

it would also withdraw these maintainer invitations:
  <eve-npub>

ask <bob-npub> to publish the complete lead roster before retrying:
  ngit repo edit --lead-maintainer <bob-npub>  # run by <bob-npub>

or explicitly remove each person before changing the lead:
  ngit repo edit --remove-maintainer <carol-npub>
  ngit repo edit --remove-maintainer <dave-npub>
  ngit repo edit --remove-maintainer <eve-npub>
```

The error names every confirmed or invited person missing from Bob's active
roster. Bob's preparation must retain them; removing them first records
Alice's separate intent. A generic force would conflate leadership semantics
with membership removal.

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
deprecated `maintainers` degradation tag containing exactly the active `M` and
`m` subjects. This includes pending assignments and excludes every inactive or
`defer` history record. Role-aware clients use the indexed records; older clients
retain the best representation available to them. Other authors' announcements
remain unchanged until those authors publish their own role or history mutation.

The `maintainers` fallback applies only when an announcement contains no
indexed `M`, `m`, or `o`. During partial migration, active indexed `M` views
from other members can still vote while the selected announcement is legacy;
historical `M` records ending in `defer` cannot. Indexed `m` without `M` in the
selected announcement is the explicit no-lead boundary.

### Moderators

In a lead-shaped repository, the lead's active `o` assigns a moderator
invitation. The recipient acknowledges it with an active self-`o`; confirmation
still requires the lead's assignment, so the recipient cannot appoint
themselves. Copied moderator history belonging to other people uses `defer`. In
a leadless repository, an active `o` from a confirmed maintainer can assign the
invitation. Moderator relationships never extend maintainer authority.

Confirmed moderators are authorized to author status, label, subject, and
cover-note events. This includes a `merge` status that records a merge commit
already present in state published by a confirmed maintainer, such as when the
maintainer's tooling pushed the commit but did not publish the corresponding
status. The moderator's status does not authorize them to create or push that
merge. Clients reject kind `30618` state from an author confirmed only as a
moderator, and moderator-authored role tags do not confer third-party roles.

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
current selected coordinate's lead-pointer path. For a confirmed maintainer it
also republishes their active self-`m`, a direct active `M` to that lead, and
non-authorizing copies of third-party history. For a removed maintainer it ends
the self-`m` but retains the active lead redirect. For other users it changes
only local configuration.

### Edge cases and failure rules

#### The compatibility roster contradicts indexed roles

Active `M` and `m` records remain authoritative when the deprecated
`maintainers` tag is absent, duplicated, missing subjects, includes inactive
subjects, or otherwise differs from their exact set. Resolution and
authorization must not fall back to the contradictory tag. For example, given
active `M:Alice` and `m:Bob`, an
`defer` `m:Carol`, and ended `m:Dave`, the only valid compatibility values are
Alice and Bob. Bob is included even while invited; Carol and Dave are excluded.
If the tag instead contains Alice, Carol, and Dave, the warning below describes
both sides of the mismatch.

When the logged-in signer is the author of the bad announcement, every
ngit or Git command in the checkout warns until it is repaired. The warning
names missing and incorrectly included pubkeys and gives one immediate command:

```text
your repository announcement has an inconsistent `maintainers` compatibility tag
indexed `M` and `m` roles are authoritative

missing active roles: <bob-npub>
listed without an active role: <carol-npub> <dave-npub>

repair only the compatibility tag with:
  ngit repo edit --fix-maintainers

this repair does not add or remove active maintainers
```

`--fix-maintainers` must be run alone. It republishes the announcement with the
projection derived from active `M` and `m`, preserving every indexed role,
history boundary, metadata field, and unknown tag. Any other `ngit repo edit`
fails before evaluating its requested change and repeats the repair command.
This prevents an unrelated edit from silently choosing whether the legacy or
indexed roster was intended.

After repairing the projection, the author may run an explicit
`--add-maintainer` or `--remove-maintainer` action if the graph permits it. If
the resolved lead's active roster itself needs to change, a co-maintainer asks
that lead to add or remove the named person first. The compatibility repair is
never a membership operation.

#### Ngit's canonical acceptance makes a required role historical-only

Ngit's canonical co-maintainer acceptance shape is incomplete if its lead `M`
or self-`m` ends in `defer`; both records must be active for that publishing
shape. Clients retain deferred records as history, but they contribute no
current edge and a deferred `M` is never used for forwarding. Read-side
confirmation still follows the reciprocal graph rule above, so a different
valid active `M` or `m` edge can provide reciprocity unless the author has
explicitly ended their own maintainer role.

If the resolved lead still has an active invitation, every repository command
for that signer reports the malformed acceptance and directs them to publish a
valid one:

```text
your announcement does not contain an active maintainer acceptance
the lead relationship and your self-role must both be active
accept the current invitation with: ngit repo accept
```

If there is no current invitation, the client reports that fact instead of
offering a command that could manufacture one.

#### A co-maintainer actively assigns a third party

The wire protocol permits a co-maintainer announcement such as this, even
though ngit must not create it in a lead-shaped repository:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2"]
["m", "<carol-pubkey>", "T3"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>", "<carol-pubkey>"]
```

Bob's active `m:Carol` is a real invitation. If Carol accepts, it extends the
reciprocal graph and can retain Carol even when Alice removes or never lists
her. Clients must resolve that graph honestly; they cannot treat the record as
`defer` merely because Bob is not lead.

The shape is nevertheless a repository-health error because it bypasses the
normal lead-coordinated roster. Bob and Alice are warned after every ngit or Git
command until Bob converges. Other commands continue according to their normal
authorization, but announcement edits are gated as described below.

When Alice already lists Carol, Bob sees:

```text
your announcement directly assigns <carol-npub> in a lead-shaped repository
let <alice-npub> manage the co-maintainer roster by running:
  ngit repo follow-lead
```

If Alice does not list Carol, converting Bob's edge to history would remove an
invitation or confirmed maintainer. `repo follow-lead` therefore refuses to
publish, and Bob instead sees:

```text
your announcement directly assigns <carol-npub>, but the lead does not
ask <alice-npub> to run:
  ngit repo edit --add-maintainer <carol-npub>
then repair your announcement with:
  ngit repo follow-lead
```

Every `ngit repo edit` from Bob other than `repo follow-lead` fails and repeats
the applicable recovery. Once Alice actively covers every third-party subject,
`repo follow-lead` converts Bob's active assignments to `defer` copies, rebuilds
`maintainers` from Bob's remaining active lead/self roles, and preserves all
history and unrelated fields:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2"]
["m", "<carol-pubkey>", "T3", "defer"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>"]
```

Alice is also warned after every ngit or Git command. When her roster is
missing a subject, the warning tells her to run the displayed
`--add-maintainer` command and then ask Bob to run `ngit repo follow-lead`.
When she already covers every subject, it tells her only to ask Bob to run that
command. Every other `ngit repo edit` from Alice fails until Bob converges; the
required one-at-a-time adds are the only lead-side exception.

For example, once Alice covers Carol the lead-side warning is:

```text
<bob-npub> directly assigns <carol-npub> instead of following your lead roster
ask <bob-npub> to repair their announcement with:
  ngit repo follow-lead
```

Before Alice covers Carol, the same warning prefixes that instruction with:

```text
first retain <carol-npub> through the lead roster with:
  ngit repo edit --add-maintainer <carol-npub>
```

After convergence Alice may keep Carol or remove her with an ordinary separate
lead action. Until convergence, Alice cannot remove Carol while retaining Bob,
because Bob's active edge remains authoritative. A force flag cannot rewrite
Bob's signed event or pretend that edge is historical.

#### Removal, replicated history, and reinvitation

Suppose Alice leads, Bob is a co-maintainer from `T2`, and Carol joins at `T3`.
After Bob synchronizes with `ngit repo follow-lead`, his announcement keeps his
own acceptance active and copies Carol's current interval as historical-only:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2"]
["m", "<carol-pubkey>", "T3", "defer"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>"]
```

Alice removes Carol at `T4`. Carol loses authority as soon as Alice's active
roster closes her assignment; Bob's copied `defer` interval cannot retain her.
Bob is prompted to run `ngit repo follow-lead`, producing:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2"]
["m", "<carol-pubkey>", "T3", "T4"]
["maintainers", "<alice-pubkey>", "<bob-pubkey>"]
```

Every repository command Carol runs reports that Alice no longer assigns her
and directs her to `ngit repo follow-lead`. Pushes and other maintainer-only
operations fail. Until Carol follows that guidance, her active self-role does
not preserve authority without Alice's assignment, but it remains standing
pre-acceptance. If Alice re-adds Carol during that interval, the two active
edges become reciprocal again and Carol is confirmed immediately without
publishing another event.

Following ends Carol's self-role, replaces a copied `defer` with a numeric end
when the lead supplies one, retains other still-current third-party records as
`defer`, and keeps Alice as an active redirect:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2", "defer"]
["m", "<carol-pubkey>", "T3", "T4"]
["maintainers", "<alice-pubkey>"]
```

The active `M` lets `nostr://<carol>/<identifier>` continue forwarding to
Alice, but the ended self-`m` means Carol is not a maintainer. Because Carol
acknowledged the removal by ending that self-role, if Alice later invites Carol
again the ended acceptance cannot confirm the new assignment. Carol must
accept again by appending the new start, here `T5`:

```text
["M", "<alice-pubkey>", "T2"]
["m", "<bob-pubkey>", "T2", "defer"]
["m", "<carol-pubkey>", "T3", "T4", "T5"]
["maintainers", "<alice-pubkey>", "<carol-pubkey>"]
```

Acknowledging removal therefore withdraws standing consent for a later
assignment. Conversely, a candidate who does not acknowledge removal retains
their active acceptance and is immediately confirmed if assigned again.

#### A removed maintainer abandons or forks the redirect

ngit provides no interface for these exceptional transitions, but clients must
interpret valid events produced elsewhere. Carol can end every relationship at
`T4`, including her redirect:

```text
["M", "<alice-pubkey>", "T2", "T4"]
["m", "<bob-pubkey>", "T2", "T4"]
["m", "<carol-pubkey>", "T3", "T4"]
["maintainers"]
```

With neither an active role nor an active lead pointer, cloning or fetching
`nostr://<carol>/<identifier>` fails instead of guessing another coordinate.

Carol can instead make an aggressive same-identifier fork by ending the old
relationships and opening a new active self-lead interval:

```text
["M", "<alice-pubkey>", "T2", "T4"]
["m", "<bob-pubkey>", "T2", "T4"]
["m", "<carol-pubkey>", "T3", "T4"]
["M", "<carol-pubkey>", "T5"]
["maintainers", "<carol-pubkey>"]
```

The same coordinate now roots Carol's new virtual repository while retaining
the signed role, issue, and proposal history up to the `T5` divergence. Carol
can invite Bob, but Bob's `(pubkey, identifier)` announcement can participate
in only one active virtual repository at a time; switching him must pass the
normal component and state-conflict checks. The friendlier fork creates a new
identifier, making the divergence explicit instead of repurposing existing
`nostr://<carol>/<identifier>` links.

#### A removal partitions the graph

`--remove-maintainer Bob` fails if Bob remains confirmed or if anyone besides
Bob loses authority. The error shows the retaining or cascading paths. Callers
then add a desired direct edge or remove each intended person explicitly.

#### Two same-identifier repositories meet

Neither add nor accept is a repository-merge command. One reciprocal edge can
join an entire transitive component, so the client compares every reachable
member and state event rather than only the two people named by the command.
It fails before an edge joins components with distinct membership, identity,
or history. When refs are the only difference, `--force` deliberately replaces
the other component's state with the command runner's current ref map; this is
a state choice, not a merge. Otherwise the operator must keep one repository
under a new identifier, align one component, or use a future merge workflow. A
merge design must cover Git history, earliest unique commits, `u` fork
relationships, ref conflicts, membership histories, local coordinates, object
availability, and recovery before exposing an explicit merge action.

#### A local repository diverges without a state event

The absence of an invitee-authored kind `30618` event does not prove that their
checkout is safe. Bob may have unpublished branches or stale local refs from
his old same-identifier repository. An explicit `repo accept` compares those
local refs before confirmation and gives the same add/update/remove guidance;
`--force` may deliberately choose them when no non-state conflict exists.

An immediately confirming add runs in Alice's checkout and cannot inspect an
offline Bob's local refs. It therefore invalidates Bob's old cache. Before
Bob's next push or automatic state publication, his client fetches the accepted
component and compares every local ref the operation would publish. If they
differ, it refuses and tells Bob to preserve the work under a new identifier or
ask the target maintainers to incorporate it. Bob's first later push never
becomes an implicit state choice.

#### Pre-existing role-scoped actions become newly visible

An acceptance's effective start prevents an earlier issue, proposal, or
moderation event from becoming valid merely because its author is a maintainer
now. A component join can nevertheless import role history containing an
earlier active interval. The preflight therefore evaluates newly reachable
maintainer and moderator events at their creation times and reports any status,
label, subject, or cover-note result that would change. Such a change blocks the
ordinary membership command and requires the same deliberate history or
component reconciliation as a state collision.

#### A history-unaware client replaces an announcement

The latest event controls the author's current statement. Its omission of a
role-history record does not erase copies retained by other confirmed
maintainers, including inactive records ending in `defer`. The preferred
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

A command rechecks the publisher's latest event, every announcement used by the
affected graph, and every candidate kind `30618` event before signing. If any
event ID changed after preview, it aborts and asks the user to rerun the intent.
This includes new state published by an invitee after the invitation or during
acceptance preflight. Addressable-event last-write-wins must not discard a
concurrent membership or state action silently. `--force` fixes the intended
state direction; it does not waive this recheck.

#### Relay disagreement

If the client cannot establish a sufficiently complete announcement and state
set to decide whether a write joins or partitions a repository, it fails
closed. The absence of an invitee's state from one relay is not proof that no
state exists, and a locally cached older event is not proof that it is still
latest. Signed deletion requests are included when determining whether an old
state remains eligible. Reads may show partial information; membership writes
require more complete evidence from the configured relay set. `--force` cannot
turn incomplete discovery into evidence that overwriting is safe.

#### The lead key is unavailable

Confirmed co-maintainers retain equal state and merge authority. They can
prepare a replacement by publishing an active self-`M` and complete roster,
subject to graph, history, and state checks. Each maintainer who still controls
an accessible coordinate can then use `repo follow-lead` to republish their
history with a direct pointer to the replacement. Replicated histories let the
replacement adopt the retained view without depending on one lead event, while
conflicting copies remain visible.

Nobody can redirect the unavailable key's own coordinate. A checkout that
already selected that coordinate cannot be auto-forwarded by somebody else's
announcement; its user must verify and select another confirmed maintainer's
coordinate explicitly. This is also the recovery rule when an organization
pubkey's secret is lost.

#### A coordinate signer changes or withdraws a forward

Alice can become lead again by first materializing a complete active roster, or
point to Carol after Carol does so. Clients rooted at `alice/my-repo` use
Alice's latest valid instruction and stop recommending Bob. Checkouts that
already followed Bob remain rooted at `bob/my-repo` and are unaffected. If
Alice's new path is pending or conflicting, clients stay on Alice's coordinate
and report the failure instead of guessing a destination.

#### An organization pubkey changes operators

Clients cannot distinguish a signature made by the intended new operator from
one made by somebody who retained the same organization secret. They must not
describe changing operators as revoking the former operator or transferring
exclusive ownership. Moving users to a newly generated lead coordinate is the
only repository-level transition that stops later changes to the old
coordinate from affecting those users.

### Authorization summary

| Actor | Repository state (`30618`) | Create/push merge commit | Status/labels/subject/cover note |
| --- | --- | --- | --- |
| Resolved lead | yes | yes | yes |
| Confirmed co-maintainer | yes | yes | yes |
| Confirmed moderator | no | no | yes |
| Invitee | no | no | no |
| Outsider | no | no | no |

Issue and proposal authors retain author-specific NIP-34 actions. The table
covers authority derived from repository roles. A `merge` status belongs in
the final column: it may record a merge already present in authorized
repository state, but it does not authorize its publisher to create that state
or perform the merge.

ngit-ci applies the same current-role boundary to continuous integration. A
configured repository coordinate first resolves its valid active `M` path,
then the terminal lead or selected leadless/legacy author seeds the reciprocal
maintainer fixpoint. A former maintainer's coordinate may therefore keep CI
attached to the current repository after a handover, but that forwarding
signer is not restored to the member set. Their kind `30618` state, global
Service Controls, build-cache trust, relays for freshness confidence, and
secret scopes remain unauthorized. Missing, conflicting, or cyclic explicit
paths seed no CI authority. Moderators receive no CI authority because ngit-ci
does not consume the status, label, subject, or cover-note events their role
permits. Legacy lead inference does not change CI permissions: `M` and `m`
remain equal authority edges, so the selected-rooted reciprocal component is
the relevant coordinator view.

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
4. Ngit's ordinary acceptance command records an active `M` naming the inviter
   and an active self-`m` naming the invitee. Both are required for that
   canonical emitted shape, and ending either in `defer` cannot satisfy it.
   Read-side authority still follows the reciprocal active-edge rule in item
   8. Acceptance cannot add unrelated people or self-promote.
5. Discovering acceptance promptly shows the lead
   `--acknowledge-maintainer-change` and shows other co-maintainers
   `repo follow-lead`, without prompting. JSON returns the same actions as
   structured data.
6. Every confirmed maintainer can replicate effective start and end intervals
   in `M`, `m`, or `o`. A co-maintainer keeps their lead `M` and self-`m`
   active, while every third-party interval whose current assignment they defer
   uses `defer` in the normal ngit shape. An externally authored active
   third-party `m` remains authoritative until the guarded repair converts it
   safely. Departure timing prefers an explicit signed role end, then a signed
   deletion request, then a clearly labelled observation estimate.
7. The selected maintainer's history wins and omissions continue through the
   sibling NIP-34 distance and pubkey precedence.
8. Active `M` and `m` have identical maintainer authority. In a lead-shaped
   repository, the lead's active roster seeds a reciprocal fixpoint and each
   confirmed candidate has a valid active `M` or `m` edge back to a confirmed
   maintainer. Ngit's canonical co-maintainer shape is an active lead `M` plus
   active self-`m`, but an active third-party `m` from any confirmed maintainer
   can extend the fixpoint even though ngit treats that wire shape as an edge
   case. A third-party `defer` history cannot. Explicit no-lead uses the
   reciprocal active-`m` fixpoint without a lead seed.
9. Lead resolution starts at the selected coordinate, follows one active `M`
   per announcement, and terminates only at a confirmed active self-`M`. An
   active pointer can preserve a removed maintainer's coordinate redirect, but
   an `M` ending in `defer` cannot route. Different targets outside that path do
   not create a global conflict.
10. Legacy listing-vote inference remains while the selected announcement is
    legacy, and active indexed `M` views still count as votes. Historical `M`
    records ending in `defer` do not. Selected indexed `m` without `M` expresses
    the no-lead choice. A membership mutation migrates the selected event to
    indexed roles while retaining the active-role degradation `maintainers`
    projection.
11. Every role-aware announcement's `maintainers` values equal exactly the
    subjects of its active `M` and `m` records, including invitations and
    excluding ended or `defer` records. Indexed roles win on disagreement. The
    author sees a warning after every ngit or Git command in the checkout, and
    every other `repo edit` fails until the standalone `--fix-maintainers`
    repair republishes only the corrected projection.
12. Every mutation preserves unrelated relationships, role intervals,
    replicated history, metadata, and unknown tags.
13. Removing a candidate from the lead roster removes them immediately when no
    other confirmed maintainer actively assigns them, even while their old
    self-`m` and lead `M` remain active. Their commands report the removal and
    direct `repo follow-lead` to end the self-role while preserving the
    redirect. A later invitation requires a new self-role start.
14. Removing one maintainer fails if that person remains confirmed through a
    different real relationship or the graph loses anyone else. An active
    third-party `m` names its author and directs the lead to cover its subject,
    ask the author to run `repo follow-lead`, and then retry any desired
    removal as a separate action.
15. In a lead-shaped repository ngit never authors a co-maintainer's active
    third-party assignment. If another client does, every command warns both
    co-maintainer and lead. The co-maintainer may run only `repo follow-lead`;
    the lead may run only the required one-at-a-time adds. Follow refuses until
    the lead covers every subject, then converts those edges to `defer` without
    changing membership.
16. A proposed lead publishes an active self-`M` and complete roster before the
    old lead points to them. A missing confirmed maintainer or invitation emits
    the required named prepare-first/remove-first error.
17. Force cannot combine lead declaration with removal or turn add/accept into
    a repository merge. On add or accept it may only choose the command
    runner's complete current kind `30618` ref map when that state difference
    is the sole remaining conflict.
18. Add resolves the named pubkey's complete reachable component, history, and
    state before publishing an edge. A pre-existing reciprocal acknowledgement
    makes the add an immediate confirmation and receives the same preflight as
    explicit acceptance. This includes a removed maintainer who has not yet
    ended their active self-role; after they acknowledge removal by ending it,
    a later add remains an invitation until they accept again.
19. Accept compares the invitee's existing announcement, earliest unique
    commit, `u` relationships, history, component, refs, and every state event
    reachable through active third-party relationships, as well as the
    command checkout's local refs. It does not drop or defer those relationships
    as an acceptance side effect.
20. Without force, confirmation cannot change either component's repository
    identity, default branch, complete ref/OID map, or resolved state event.
    A state-only mismatch reports every branch and tag to add, update, or remove
    in the command runner's repository and the inverse changes force would
    impose. All post-change OIDs must be fetchable, and a forced state event
    must order after every state candidate used by the preview.
21. Successful confirmation, including an immediately confirming add,
    invalidates state derived from the invitee's former component. No push or
    automatic kind `30618` publication is allowed until a fresh fetch verifies
    the accepted state against the local refs.
22. Membership preflight rechecks every announcement and kind `30618` event ID
    used by the preview immediately before signing. A concurrent graph or state
    change aborts the operation.
23. An unexpected component join or newly authorized role-scoped action always
    blocks before signing and reports every transitively imported pubkey. A
    state-only conflict blocks unless `--force` publishes and verifies the
    command runner's previewed state without changing membership beyond the
    named confirmation or changing repository identity.
24. A lead transfer never rewrites announcements or local coordinates
    automatically. Human-facing commands repeatedly offer `repo follow-lead`
    until each co-maintainer has an active direct `M` to the new lead, an active
    self-`m`, and a local coordinate that follows it; non-maintainers update
    only local configuration.
25. A coordinate remains controllable by every holder of its signing key;
    changing its lead cannot transfer or revoke that control.
26. Metadata-only edits do not migrate legacy membership.
27. A historical copy can never authorize its subject or route lead resolution
    when its final interval is `defer`.
28. ngit exposes no command to abandon a removed maintainer's redirect or turn
    the same coordinate into a new self-led virtual repository. Clients still
    interpret those externally authored events deterministically and recommend
    a new identifier for a friendly fork.
29. Current authorization remains defined when exact history is missing or
    disputed.

Each normal workflow and destructive edge case needs a unit-level graph,
history, and state fixture plus an integration test for the published
announcement, selected coordinate, and resulting authorization. Tests wait on
observable relay or Git state with bounded deadlines and never use fixed
sleeps. The compatibility-roster fixture specifically covers an active
invitation, a record ending in `defer`, an ended record, absent and contradictory
projections, a mismatch warning, the edit gate, and a repair that leaves all
indexed role tags byte-for-byte unchanged. The reciprocal-lifecycle fixtures
cover active lead/self acceptance, rejection of `defer` in either required
record, passive third-party `defer` copies, correct resolution of externally
authored active third-party `m` assignments, warnings to co-maintainer and lead,
both edit gates, refusal to follow before lead coverage, safe conversion after
coverage, rejection of a lead removal while such an edge retains its subject,
immediate removal without such an edge, the removed-author warning and follow
repair, reinvitation requiring a new self-role start, a dead coordinate after
an externally authored redirect end, and an externally authored
same-identifier self-led fork.

The state-collision fixtures cover an invitee's older state winning, the
inviting state winning, an add that confirms immediately, a transitive
invitee-to-third-party component import, compatible state with distinct event
authors, unavailable Git objects, divergent local refs without a published
state event, a candidate state change after preview, and incomplete relay
visibility. Every blocking case verifies that no announcement or state event
was published. State-only failures verify the exact branch, tag, and default-
branch alignment actions in both directions. Forced cases verify that add and
accept publish the command runner's complete state, while force remains rejected
for extra members, identity or history conflicts, missing objects, concurrent
changes, inability to order the replacement after every candidate, and
incomplete discovery.
