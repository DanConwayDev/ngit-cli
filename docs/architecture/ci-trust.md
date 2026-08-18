# CI Status and Trust Context

**Status:** design — not yet implemented. Written as a build plan: each work
package below is independently buildable and reviewable.

## Purpose

Surface NIP-34 CI results (workflow runs, conclusions, progress) inside ngit,
attached primarily to PRs, with every result labelled by the CI trust-context
model so a signature is never silently presented as maintainer endorsement.

## Normative sources

- **Event shapes:** `NIP.md` and `NIP-guidance.md` in the `ngit-ci` repository.
  Kind numbers are experimental placeholders; centralize them in one module and
  surface incompatible shapes rather than reinterpreting them.
- **Trust semantics and canonical wording:** `docs/ci-trust-context.md` in the
  `gitworkshop` repository. Its "Recommended language" strings are canonical;
  ngit must reuse them verbatim so CLI and web agree.
- **Reference implementation to port:** gitworkshop
  `src/lib/ciTrustContext.ts`, `src/lib/ciCoordinatorRelationship.ts`, and the
  evidence-assembly logic in `src/hooks/useCITrustContext.ts`.
- **Maintainer model:** `docs/architecture/maintainer-model.md` in this
  repository. Trust evidence requiring "a confirmed repository maintainer"
  means `RepoRef::confirmed_maintainers()` — never invited maintainers.

## Event vocabulary (consumed subset)

| kind  | event                | role in this design                          |
| ----- | -------------------- | -------------------------------------------- |
| 9842  | Workflow Result      | terminal outcome; quotes Job Results and provenance |
| 39842 | Workflow Progress    | queued/in_progress/concluded; NIP-40 expiry ≤ 30 min |
| 9841  | Job Result           | per-job outcome, provider-signed             |
| 9843  | Service Request      | maintainer-directed evidence (standing)      |
| 9844  | Service Stop         | closes requests; total order                 |
| 9840  | Manual Trigger       | maintainer-directed evidence (one-shot)      |

PR-triggered CI events carry NIP-22 tags: `E` = kind-1618 PR root, `e` = PR or
kind-1619 update that supplied the commit, `c` = commit id. Push/tag-triggered
events carry an `r` Git-ref tag instead. Attaching CI to a PR is therefore a
`#E` filter; to a commit, a `#c` filter.

Out of scope for now: Repository Secret Updates (29846), Coordinator
Advertisement/Readiness/Status (19843/19844/39844) beyond what provenance
validation needs, and NIP-11 operator-key strengthening.

## Trust model — port requirements

Port the gitworkshop model 1:1 unless noted. The load-bearing rules:

1. **Evidence, not scores.** A resolution is a list of typed evidence items
   `{ kind, classification, summary, detail, authors, scope }`. Classification
   of a signer = strongest evidence item; empty list = `NoKnownContext`
   (absence of evidence, never "untrusted").
2. **Classifications:** `MaintainerDirected` > `OperationallyAssociated` >
   `SociallyCorroborated` > `NoKnownContext`. Level 3 (social) is deferred —
   see phasing — but the enum includes it from the start.
3. **Temporal correctness.** A *current* standing Service Request never
   retroactively covers old runs. Per-run coverage is derived from either
   (a) the run's frozen `service-request` / `manual-trigger` `q` quote,
   validated (below), or (b) reduction of the immutable control history at the
   run's `started_at` (fallback `queued_at`), using the NIP's total order:
   greater `created_at` is later; at equal timestamps the lexicographically
   lower event id is later. A Stop from a confirmed maintainer closes the whole
   perspective; any other author's Stop closes only their own requests.
4. **Quote validation.** A coordinator-authored `q` tag is not evidence. The
   quoted 9843/9840 must be fetched and checked: event id, kind, author is a
   confirmed maintainer, `a` repository coordinates intersect the resolved
   maintainer closure, `p` names the coordinator, and (for manual triggers) the
   run context matches. Only then does it yield `MaintainerDirected` evidence
   with `scope: run`.
5. **Delegation scoping.** When a Job Result is signed by a different pubkey
   than the coordinator, coordinator trust reaches the provider only through
   the Workflow Result that accepts that job, only for that job, and downgraded
   (never above `OperationallyAssociated`, except socially-corroborated stays
   socially-corroborated). Provider evidence never flows back to the
   coordinator.
6. **Rollups are conservative.** Summarizing several runs (e.g. one table
   cell) surfaces the *weakest* settled run, and any partial coverage makes the
   rollup partial.
7. **Domain evidence (Level 2).** Verified NIP-05 identities — the signer's
   kind-0 `nip05` and the synthetic root candidates `_@<grasp-domain>` for
   every GRASP domain in the resolved repository's clone URLs — matched against
   those domains. Exact match is direct association; proper parent/child
   subdomain (DNS-label boundaries only, ports and trailing dots stripped) is
   weaker and must say so; sibling domains are not evidence. Never use string
   suffix matching without label boundaries.
8. **Resolution ≠ classification.** States: `loading`, `settled+complete`,
   `settled+partial`. A signer must never display "No known context" while
   evidence queries are unsettled; failed relays/lookups settle as partial with
   the canonical caveat **Context incomplete**.

ngit-specific addition: **local integrity check**. ngit holds the git objects,
so for each run it can verify the `c` commit exists locally (or in fetched PR
refs) and that the `w` tag's SHA-256 matches the workflow file blob at that
commit. Report as a separate integrity marker alongside trust — it is not a
trust level, and the trust doc is explicit that trust context does not prove
source integrity.

## Library design: `src/lib/ci/`

- `kinds.rs` — kind constants and strict shape validation for consumed kinds.
- `events.rs` — parse events into `WorkflowRun { coordinator, workflow_path,
  workflow_hash, trigger, pr_root, supplying_event, commits, progress, result,
  jobs }`; group Progress/Result/Job Results by workflow-run id; latest attempt
  per (coordinator, workflow) is current; respect Progress expiration (expired
  progress with no result = stale, not running).
- `controls.rs` — Service Request/Stop reduction and coordinator relationship
  tiers (`requested` / `previously-requested` / `unassociated`); port of
  `ciCoordinatorRelationship.ts` including `wasCIServiceRequestedWhenRunStarted`.
- `provenance.rs` — frozen-quote fetch + validation (rule 4).
- `domain.rs` — NIP-05 resolution (reuse the existing nostr-URL NIP-05 fetch
  path) and the domain-relationship ladder (rule 7), with a TTL cache in the
  local cache dir.
- `trust.rs` — classification/evidence types, strongest-evidence
  classification, run/job resolution (rules 3–5), weakest rollup (rule 6),
  canonical label/wording constants.
- `resolve.rs` — assembly with **two tiers**:
  - *cache tier*: evidence computable from the local nostr cache alone
    (control-history reduction, quote validation when the quoted event is
    cached). Used by `pr list`. Coverage is `partial` when domain checks were
    skipped.
  - *full tier*: additionally fetch missing quoted events and perform NIP-05
    domain verification. Used by `pr view`, `pr merge`, `ci status`.

Fetching: add the consumed CI kinds (9840–9844, 9841/9842, 39842) to the
repo-wide filters in `fetching_with_account`
(`src/bin/ngit/sub_commands/repository_fetch.rs` path), so CI events land in
the normal local cache during the fetch every PR command already performs.

## Per-PR CI state machine

For the PR's **latest revision** (root 1618 or newest 1619 tip):

- `running` — unexpired 39842 with status queued/in_progress.
- `concluded` — 9842 present; conclusion rolls up worst-of across workflows
  (`failure`/`timed_out`/`startup_failure` beat `cancelled` beat `success`;
  `neutral`/`skipped` do not fail the rollup).
- `stale` — only expired progress, no result.
- `none` — no CI events reference the PR.

Results for earlier revisions are never presented as current; the detail view
lists them under an "outdated" heading.

## CLI surfaces

### `ngit ci status <target>` (new command)

`<target>` resolution order (documented in `--help`):

1. `#<hex-prefix>` — always a PR/event prefix; resolve against cached PR roots
   via `resolve_pr_root_or_prefix` (`src/bin/ngit/sub_commands/id_resolver.rs`).
2. nevent / note bech32 or full 64-char hex — always an event id
   (`parse_event_id`); must resolve to a cached PR root (or revision root,
   which maps to its PR root).
3. otherwise — try `git rev-parse` as a commit-ish; on success query by `#c`
   (including annotated-tag peeling: query both tag and peeled commit ids).
4. bare short hex that fails rev-parse falls back to PR-prefix resolution; the
   ambiguity error must suggest `#<prefix>` for events.
5. no target — HEAD commit.

Output: one line per workflow run — conclusion, workflow path, trust label,
one-line evidence summary — plus integrity marker and a trailing
`context incomplete` caveat when coverage is partial. Exit code 0; with
`--require-ci-trust=<maintainer-directed|operationally-associated>` exit
non-zero when the rolled-up current result does not meet the floor or is not
`success`. Supports `--offline` (cache tier only) and JSON output.

### `ngit pr view <id>`

Add a "Checks" section: current-revision runs as in `ci status` (full tier),
older revisions collapsed as outdated, delegated jobs listed with their
per-job resolution.

### `ngit pr list`

Add a `CI` column computed from the cache tier:

```
✓   passing, trust floor met (operationally-associated or better)
✓?  passing, but only signers with no known context
✗   failing (any trust level — a failure is a prompt to look, not a verdict)
…   running    ~ stale    -   none
```

One footer line explains `?` and points at `ngit pr view`. No network beyond
the shared fetch; no NIP-05 lookups from the list path.

### `ngit pr merge <id>`

Print the Checks summary before merging. `--require-ci-trust` as above;
without it, warn (non-blocking) when the current result is failing, stale, or
below the floor.

### JSON shape (all surfaces)

```jsonc
"ci": {
  "state": "running|concluded|stale|none",
  "conclusion": "success|failure|...",        // when concluded
  "revision_matched": true,
  "coverage": "complete|partial",
  "runs": [{
    "workflow": ".ngit/act/workflows/ci.yml",
    "conclusion": "success",
    "attempt_of": 1,
    "coordinator": "<pubkey>",
    "classification": "maintainer-directed",
    "evidence": [{ "kind": "maintainer-request", "classification": "...",
                    "summary": "...", "authors": ["<pubkey>"], "scope": "run" }],
    "integrity": { "commit_present": true, "workflow_hash_matches": true },
    "jobs": [{ "job": "build", "conclusion": "success",
               "provider": "<pubkey>", "classification": "..." }]
  }]
}
```

Integration tests assert on this JSON and exit codes, never on table text.

## Work packages

Each WP is one reviewable unit with its own tests. Build in order; WP2+ depend
on WP1.

- **WP1 — `src/lib/ci/` core** (`kinds`, `events`, `controls`, `trust`):
  parsing, run grouping, control reduction, classification, rollups, temporal
  rules. Pure unit tests ported from the semantics above (total-order
  tie-break, retroactivity, stop scoping, weakest rollup, progress expiry).
- **WP2 — provenance + domain + resolve tiers**: quote validation, NIP-05
  domain ladder with TTL cache, cache/full tiers, coverage states. Unit tests
  for the domain ladder (label boundaries, ports, trailing dots) and coverage.
- **WP3 — `ngit ci status`**: target resolution (order above), fetch-filter
  extension in `fetching_with_account`, human + JSON output, integrity check,
  `--require-ci-trust`, `--offline`. Integration tests: publish fixture CI
  events via the test harness, assert JSON/exit codes for commit-ish, `#prefix`,
  nevent, and full-hex targets, including the ambiguity error.
- **WP4 — `pr view` Checks section + `pr list` CI column**: cache-tier
  projection, revision matching, outdated grouping. Integration tests via JSON.
- **WP5 — `pr merge` gating**: summary, warning, `--require-ci-trust` exit
  behavior. Integration tests: merge blocked/allowed matrices.
- **WP6 (later) — maintainer controls**: `ngit ci request|stop|trigger`
  publishing 9843/9844/9840 (trigger computes `w` hash from the local blob and
  peel-verifies `c` tags). These create the Level 1 evidence WP1 consumes.

Deferred beyond WP6: social corroboration (Level 3) for logged-in users,
secrets provisioning (29846), NIP-11 strengthening, courtesy CI lines in
`pr checkout`/`pr apply`.

## Test-harness constraints (mandatory)

Per `docs/architecture/test-harness.md` and `AGENTS.md`: no `#[serial]`, no
PTY/interactive prompts (drive via flags), no `std::env::set_var`, no exact
stdout assertions (assert on JSON output, exit codes, and events on relays),
push via `Repo::nostr_push`, and no wall-clock sleeps — CI fixture events are
published directly to the harness relay, so results are queryable immediately.
