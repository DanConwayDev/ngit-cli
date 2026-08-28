# Maintainer Model Implementation Plan

This document defines the delivery boundary for the maintainer model in the
next major ngit release. The normative design remains
[`maintainer-model.md`](maintainer-model.md). This plan distinguishes the API
and behaviour that must ship with the major release from support that can be
added later without replacing that contract.

## Release strategy

The major release establishes the new CLI, JSON, wire-format, and
authorization contracts. It implements the ordinary lead-shaped workflow and
refuses complex mutations before publication when it cannot apply them safely.
Later releases may turn those refusals into supported workflows and add richer
diagnostics and tests.

Authorization in this release uses the **current resolved active roster for
all events**. Role timestamps are parsed, preserved, and emitted, but an
event's `created_at` is not yet compared with historical membership intervals.
History-aware Issue and proposal interpretation is explicitly deferred.

## Current branch foundation

The indexed-maintainer-role branch already provides the base to build on. It
currently:

- parses and emits `M`, `m`, and `o` role tags and numeric role history;
- resolves reciprocal current maintainers and moderators;
- filters repository state to confirmed maintainers and authorizes confirmed
  moderators for member actions;
- parses and emits an explicit lead `M` and exposes `--lead-maintainer` through
  the existing repository publication path;
- implements explicit `repo accept` and `repo leave` commands;
- exposes roles, invitations, sources, and moderators through `ngit repo`;
- retains the legacy `maintainers` fallback; and
- provides fabricated role-graph fixtures and integration coverage for
  acceptance, role display, moderator authority, and rejected state pushes.

This work remains on the current branch. The implementation sequence below is
forward work from its present `HEAD`; it does not require splitting the branch,
reordering its existing commits, or recreating already completed tests.

## Remaining implementation sequence

Implementation status at the branch head: Waves 1–5 are complete. Wave 6 has
stabilized the documented CLI, structured lead and health output, categorized
membership errors, bundled skill guidance, and fail-closed collision checks.
The items under “Deliberately deferred” remain intentionally outside this
major-release boundary.

Each wave ends green and includes focused tests for its changed contract.
Tests may be written red-first locally, but commits contain the implementation
and its passing tests together.

### Wave 1: finish current-role and lead resolution

Make the existing role resolver the single current-roster authority:

1. Represent role boundaries explicitly as numeric timestamps or `defer` in
   production parsing and the test harness.
2. Treat `defer` as inactive for its tag while preserving it verbatim when
   history is republished.
3. Replace the current global unique-`M` lookup with a result that records the
   lead, source, and selected-coordinate path.
4. Support implicit-sole, explicit forwarding, legacy-inferred, explicit-none,
   none, pending, and conflict results. Restore unique-winner legacy vote
   inference and retain leadless legacy ties.
5. Derive the compatibility `maintainers` value from the active `M`/`m`
   projection.
6. Route state selection, push authorization, and member-event authorization
   through the resolved current roster.

Use table-driven resolver tests for the role and lead shapes. Keep integration
coverage to the existing consumer tests plus focused regressions where a
consumer previously bypassed the resolver.

### Wave 2: remove implicit and file-driven membership writes

Remove membership changes that occur as side effects of unrelated commands:

1. Stop pushes from auto-accepting an invited maintainer.
2. Stop Issue and proposal status, label, subject, and cover-note commands from
   publishing an acceptance.
3. Require explicit `repo accept` before a maintainer-only push succeeds.
4. Stop a pushed `maintainers.yaml` from replacing announcement membership;
   retain it only as the documented coordinate fallback.
5. Ensure rejected pushes may still deliver proposal refspecs but never
   publish repository state or an announcement for the rejected signer.

Invert the existing auto-accept integration test and retain the current mixed
proposal/state push coverage. Rejection tests assert the absence of kind
`30617` and `30618` side effects rather than exact terminal output.

### Wave 3: establish the major-version repository-edit API

Replace the shared init/edit membership surface with the final public shape:

1. Give `ngit init` and `ngit repo edit` distinct argument types.
2. Remove `--other-maintainers` and reject it at the CLI boundary.
3. Add one-at-a-time `--add-maintainer` and `--remove-maintainer` actions.
4. Add the `--lead-maintainer` and `--no-lead-maintainer` governance choices
   and their conflict and requirement rules.
5. Add `--acknowledge-maintainer-change` as a standalone history action.
6. Permit metadata edits without a membership action, but permit at most one
   relationship action per invocation.
7. Define stable structured error categories and preserve no-event-on-error.

The first add by a sole maintainer assigns that publisher as lead unless the
same command explicitly chooses no lead. A lead-shaped repository directs
roster changes through its resolved lead. A legacy or explicitly leadless
multi-maintainer view requires the documented governance choice.

### Wave 4: implement the normal roster lifecycle

Implement the wire transitions behind the new API:

1. Add one pending maintainer without authorizing them.
2. Accept with an active `M` pointing to the lead, an active self-`m`, and
   other replicated role records ending in `defer`.
3. Acknowledge an observed acceptance by replacing the invitation start with
   the acceptance start.
4. Remove one maintainer by closing that relationship without altering any
   unnamed relationship.
5. Preserve and close the correct per-letter history when a member leaves,
   returns, becomes lead, or ceases to be lead.
6. Emit `maintainers` from only the active `M`/`m` records; `defer` copies never
   appear in it.
7. Support a direct handover only after the proposed lead actively lists the
   complete current roster.

If add, accept, remove, or handover detects extra membership, a joined
same-identifier component, divergent state, a pending/conflicting lead, or an
incomplete view, it fails before signing. The richer reconciliation described
under deferred work is not improvised in this wave.

Extend the existing `repo_accept`, role-history, and lead-collapse tests rather
than creating a second end-to-end matrix. Replace the old forced-collapse
expectation with the named-person removal guidance.

### Wave 5: add normal-path `repo follow-lead`

Add `ngit repo follow-lead` for a fully resolved, unambiguous lead path with
equivalent current state:

1. Show the follow command when the selected coordinate safely resolves to a
   different lead coordinate.
2. For a confirmed co-maintainer, keep the active self-`m`, point an active
   `M` directly to the lead, and copy other known history with `defer`.
3. For a removed maintainer, end the active self-role while retaining the lead
   redirect and copied history.
4. For a user without a repository role, change only local repository
   selection.
5. Update the selected `nostr://` remote and `nostr.repo` consistently so later
   fetches and commands use the lead coordinate.
6. Refuse pending, conflicting, state-divergent, or incompletely discovered
   paths without changing the announcement or local configuration.

Cover the three caller roles and the no-local-change-on-refusal invariant with
focused integration tests.

### Wave 6: release stabilization

Before the major release:

1. Finalize `ngit repo --json` lead-source, lead-path, membership, pending
   action, and health fields.
2. Update bundled skill guidance and command help to describe only behavior
   that ships.
3. Reconcile existing tests whose names or expectations still describe roster
   replacement, forced collapse, or implicit acceptance.
4. Run formatting, the complete test suite, and clippy with warnings denied.
5. Verify every detected deferred mutation fails before publication and local
   configuration changes.

The major-version implementation is complete when every item under “Included
in the major release” is implemented, the supported normal workflows pass end
to end, and detected deferred cases fail closed. Exhaustive edge-case support
is not a release condition.

## Included in the major release

### Public command surface

Repository creation and membership editing become separate APIs:

- `ngit init` creates a sole-maintainer repository and edits creation-time
  repository metadata. It does not publish a replacement maintainer list.
- `ngit repo edit` edits repository metadata or applies one named membership
  change.
- `--other-maintainers` is removed. No command accepts a complete replacement
  maintainer list.
- `--add-maintainer <npub>` and `--remove-maintainer <npub>` change one
  relationship at a time.
- `--lead-maintainer <npub>` selects a lead. It may accompany an add or remove
  when the governance choice is required.
- `--no-lead-maintainer` explicitly permits an add or remove in the
  deliberately leadless model. It is required for each such mutation when no
  lead is set.
- `--acknowledge-maintainer-change <npub>` records an observed acceptance or
  removal in the publisher's retained history.
- `ngit repo accept` explicitly accepts an invitation.
- `ngit repo leave` ends the caller's own active role.
- `ngit repo follow-lead` follows an unambiguous resolved lead and performs the
  normal announcement update described below.

Membership options are non-interactive. An omitted governance decision or an
unsafe transition produces an actionable error rather than a prompt.

`--force` does not permit unnamed membership changes, roster collapse, graph
merges, or discarded history. Its future membership use is reserved for the
state-only replacement described by the model. Until that preflight is
implemented, such a collision remains an error even when `--force` is passed.

### Wire format and compatibility

The release:

- parses and emits indexed `M`, `m`, and `o` role records;
- treats a numeric-ended record and a record ending in `defer` as inactive for
  that announcement;
- gives indexed roles precedence over `maintainers` when both are present;
- emits `maintainers` as the exact current active `M`/`m` projection for
  compatibility;
- requires reciprocal announcements before an invitation grants authority;
- retains numeric role history when republishing an announcement;
- continues to read legacy `maintainers`-only repositories; and
- retains unique-winner legacy vote inference when no indexed lead decision
  has replaced it.

An indexed `m` view without an active `M` is an explicit no-lead view. It does
not silently fall back to legacy lead inference.

### Current-roster authorization

The current resolved roster is the authorization source for repository state
and collaboration events:

- confirmed `M`/`m` maintainers may publish kind `30618` state, push Git state,
  merge, and perform moderation actions;
- confirmed `o` moderators may publish Issue, proposal, and patch status,
  label, subject, and cover-note events, including an applied or merge status;
- moderators may not publish kind `30618` state or create or push a merge;
- pending invitees and unacknowledged moderators have no repository role
  authority; and
- an Issue or proposal author retains the existing authority over their own
  item.

Invitations are accepted only by `ngit repo accept`. Git pushes, status
changes, labels, subject changes, and cover-note changes do not publish an
acceptance as a side effect.

`maintainers.yaml` remains a legacy coordinate fallback when no Nostr
coordinate is configured. A push does not treat it as a membership or history
update.

### Normal workflows

The release supports these paths end to end:

1. A sole maintainer creates a repository.
2. The sole maintainer adds one person and becomes lead automatically unless
   `--no-lead-maintainer` is supplied.
3. The invitee explicitly accepts as a co-maintainer.
4. The lead acknowledges the acceptance timestamp.
5. A lead adds or removes one maintainer without changing anybody else.
6. A maintainer leaves by ending their own role.
7. A repository deliberately remains leadless when each mutation repeats the
   explicit no-lead choice.
8. A legacy repository continues to resolve its reciprocal roster and
   inferred lead, then records an explicit governance choice on its first new
   membership mutation.

A direct, unambiguous lead handover is supported only after the proposed lead
lists the complete active roster. The old lead then points to the new lead,
and co-maintainers can converge with `ngit repo follow-lead`.

For a confirmed co-maintainer, `repo follow-lead` keeps their own `m` and the
resolved lead `M` active, copies other known role history using `defer`, and
updates the checkout's selected coordinate. For a removed maintainer it ends
the self-role while retaining the lead redirect. For a user without a role it
changes only the local selected coordinate. The command refuses pending,
conflicting, state-divergent, or incompletely discovered paths.

### Stable machine-readable surface

`ngit repo --json` distinguishes:

- the selected maintainer and selected coordinate;
- active, invited, and moderator membership;
- the resolved lead, lead source, and traversed lead path; and
- pending actions and repository-health problems.

Lead-source values reserve the complete model vocabulary: `implicit_sole`,
`explicit`, `legacy_inferred`, `explicit_none`, `none`, `pending`, and
`conflict`. Later releases may add fields and diagnostic detail without
renaming these meanings.

Mutation errors have stable categories and identify the named relationship.
No-event-on-error is part of the contract: a rejected membership command does
not publish an announcement or state event and does not change Git refs or the
selected coordinate.

### Test boundary

The release reuses and updates the existing test suite to cover:

- active, ended, and `defer` role parsing;
- reciprocal confirmation and legacy fallback;
- unique legacy lead inference and leadless legacy ties;
- the maintainer/moderator authorization boundary;
- explicit acceptance and the absence of auto-accept side effects;
- the normal add, accept, acknowledge, remove, leave, handover, and
  follow-lead workflows;
- rejection of the removed roster-replacement CLI; and
- absence of publications and local mutations on refused operations.

The goal is confidence in the supported contract, not an exhaustive
cross-product of every graph and event shape.

## Deliberately deferred

The following work remains governed by the full maintainer model but is not
required for the first major-version implementation:

- authorizing Issue, proposal, patch, and moderation events against membership
  at each event's `created_at` rather than the current active roster;
- restoring historical actions that were valid during a now-ended membership
  interval;
- complete state-collision previews, per-ref reconciliation, and the narrow
  `--force` state-replacement workflow;
- accepting or adding a maintainer when doing so would join another
  same-identifier repository component;
- explicit repository-adoption and multi-component merge workflows;
- aggressive same-identifier fork workflows;
- automatic reconciliation of concurrent or relay-divergent membership
  histories;
- complete handling and repair of active third-party assignments authored by
  a co-maintainer;
- persistent health warnings and the standalone compatibility-roster repair
  command;
- a public moderator-assignment API; and
- exhaustive scenario-matrix integration coverage.

Until a deferred mutation is supported, ngit should fail before signing when
it detects that the operation depends on that behaviour. Supporting a
previously refused case is additive; silently publishing a guessed result is
not.
