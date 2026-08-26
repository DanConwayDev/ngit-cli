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
