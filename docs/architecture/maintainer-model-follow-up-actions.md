# Maintainer Model Follow-up Actions

This document tracks work still needed to reach the desired final behavior in
[`maintainer-model.md`](maintainer-model.md). The wire format is specified by
the sibling NIP-34 draft in `../nips/34.md`.

The current maintainer-model PR establishes the major-version API and the
ordinary lead-shaped workflow. Unsupported graph and state transitions fail
closed before publication. A follow-up may make one of those transitions
safe, but must not silently guess at membership, history, or repository state.

## PR review gate

- Review the complete PR against `maintainer-model.md`, treating that document
  as the desired final outcome and `../nips/34.md` as the protocol
  specification.
- Classify every discrepancy as one of:
  - a wire-format or API incompatibility that must be fixed before the major
    release;
  - an additive client workflow or protection suitable for follow-up; or
  - a disagreement that requires changing the model or NIP before changing
    code.
- Confirm that every detected unsupported mutation fails before signing,
  publishing, changing refs, or changing the selected coordinate.

## Historical authorization and replicated history

- Authorize Issue, proposal, patch, and moderation events against membership
  at each event's `created_at`, rather than only against the current roster.
- Restore historical actions that were valid during a now-ended membership
  interval without reviving actions that were unauthorized when published.
- Detect signed acceptance, removal, and departure transitions promptly and
  expose the required acknowledgement or `repo follow-lead` action in human
  and JSON output.
- Complete departure-boundary handling for explicit role ends, signed deletion
  requests, and clearly labelled observation estimates.
- Reconcile concurrent or relay-divergent history without discarding a
  maintainer's retained intervals.

## Persistent health and repair guidance

- Warn on subsequent ngit and Git commands while a maintainer has an
  acknowledgement, removal, malformed acceptance, or lead-follow action
  outstanding.
- Detect a `maintainers` compatibility tag that disagrees with active `M` and
  `m` roles, and add the standalone `ngit repo edit --fix-maintainers` repair.
- Detect active third-party assignments authored by a co-maintainer. Guide the
  lead to cover the named people and the co-maintainer to convert those records
  to `defer` with `repo follow-lead`.
- Expand `ngit repo --json` health and pending-action output, and provide stable
  categories for every guarded membership refusal.

## Complete state-safe membership preflight

- Reconcile a maintainer's existing same-identifier announcement during
  acceptance without losing metadata, personal infrastructure, `r`/`u`
  identity, unknown tags, moderator acknowledgement, or numeric/deferred role
  history. Preserve a still-active self-role as standing acceptance when a
  removed maintainer has not acknowledged the removal; after they end that
  self-role, reinvitation must append a fresh acceptance interval.
- Resolve the complete reachable component on both sides of add and accept,
  including announcements, role history, repository identity, `r` and `u`
  relationships, state events, default branch, and the complete ref/OID map.
- Report exact branch and tag additions, updates, and removals in human output
  and as structured JSON actions.
- Check newly authoritative maintainer and moderator actions and verify that
  every selected OID is fetchable from the resulting component.
- Recheck every announcement and state-event ID used by the preview immediately
  before signing.
- Implement the narrowly scoped add/accept `--force` workflow that keeps the
  command runner's complete state only when state is the sole conflict. Publish
  and verify a fresh ordered state event without changing unnamed membership.
- Invalidate state inherited from an invitee's former component after
  confirmation and require a fresh fetch before publishing new state.

## Repository-component workflows

- Resolve an explicit lead from a selected forwarding coordinate whose author
  has ended their own role, without treating that selected non-member as an
  authority seed. Until this topology is supported, repository data and member
  actions reached only through it remain fail-closed.
- Support deliberate add or acceptance when it would join another
  same-identifier maintainer component.
- Define explicit repository adoption and multi-component merge workflows,
  including reconciliation of Git history, refs, identity, infrastructure,
  membership history, and imported maintainers.
- Interpret aggressive same-identifier forks deterministically while
  recommending a new identifier for friendly forks.

## Additional public API

- Add one-at-a-time moderator assignment and removal commands with the same
  reciprocal confirmation and preflight guarantees as maintainers.
- Add any reconciliation commands needed by the component workflows above
  without reintroducing complete-roster replacement.

## Validation

- Add unit-level graph, history, and state fixtures for each normal workflow
  and destructive edge case in the model.
- Add focused integration scenarios for published announcements, selected
  coordinates, authorization results, and the absence of publication or local
  mutation on refusal.
- Keep tests parallel-safe and wait only on observable conditions with bounded
  deadlines.
