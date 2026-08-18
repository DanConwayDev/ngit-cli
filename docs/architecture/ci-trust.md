# CI Status and Trust Context

**Status:** WP1 (`src/lib/ci/` core), WP2 (`provenance`, `domain`,
`resolve`), WP3 (`ngit ci status`), WP4 (`pr view` Checks, `pr list` CI
column) and WP5 (`pr merge` gating) implemented; WP6 onwards are design.
Written as a build plan: each work package below is independently buildable
and reviewable.

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
   quoted 9843/9840 must be fetched and checked: event id, kind, a valid
   signature, author is a confirmed maintainer, `a` repository coordinates intersect the resolved
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

### WP1 implementation decisions

Recorded where the build resolved something this design or the TypeScript
reference left open:

- **A frozen quote is only evidence once validated.** Rule 4 is enforced at
  this layer by construction: `run_maintainer_link` and
  `run_trust_resolution` take a caller-supplied `validated_provenance` set of
  quote ids that WP2 has fetched and checked, and report maintainer direction
  only for a quote in that set whose requester is a confirmed maintainer.
  With an empty set — every caller until WP2 lands — an unvalidated quote
  contributes nothing, and only the control-history reduction can establish
  maintainer direction.
- **Strict shapes, skipped with a reason.** `kinds.rs` rejects an event that
  breaks a NIP MUST rather than reinterpreting it: a Job Result quoting a
  Service Request; a *queued* Progress carrying the `service-request` quote
  that is only frozen at runner handoff; a provenance quote without its
  required requester pubkey hint; a Workflow Result without the non-`refs/`
  workflow-run `r`; a Job Result without exactly one quoted Workflow
  Progress address; empty-content and cardinality MUSTs (9843/9844 `a`/`p`,
  Progress `d`/`status`/`expiration`, `conclusion` only when concluded).
  `events.rs` reports each rejection with its reason instead of dropping it
  silently. Two of these bounds are ngit's, not the NIP's, and are noted as
  such: the Progress `expiration` must be *after* `created_at` (the NIP only
  caps it at 30 minutes, and a marker expiring before it was signed is
  meaningless), and repeated `a` tags must share one repository identifier.
  Everything the NIP does not forbid is tolerated: unmarked or unrecognised
  `q` entries, extra `p` tags on result-like events (the first NIP-22 `p` is
  the parent author), and a non-`refs/` `r` on a Progress marker are ignored,
  not rejected. The exception is a non-PR Manual Trigger with more than one
  `p`: the NIP has coordinators require exactly one `p` for themselves, and
  the trigger's coordinator is later checked by membership, so tolerating a
  second `p` would let a coordinator the maintainer never addressed pass
  provenance validation.
- **Run identity is the workflow-run id.** Grouping keys on
  `(coordinator, run id)` only. gitworkshop's `queued_at`-plus-context
  fallback for publishers that predate the `r` run id is not carried over;
  ngit requires the run id.
- **Job Results are grouped by their quoted 39842 address**, which is the
  provider's own statement of the run. A job whose run has no Result or
  Progress marker is skipped as orphaned. This is *not* the NIP's "clients
  MUST NOT require a quoted Workflow Progress event to remain available after
  its expiration in order to accept a Job Result": an expired Progress marker
  is irrelevant here, because a run is retained as soon as either container
  was seen, and the coordinator's Result outlives the marker. The skip only
  covers a Job Result whose run ngit has never seen a container for, where
  the repository association would rest on the provider's own `a` tags alone.
  If a real deployment loses containers often enough for that to hide
  results, surfacing provider-only runs — clearly labelled — is the intended
  follow-up.
- **A coordinator's acceptance decides which job claim represents a job.**
  Where the Workflow Result quotes a Job Result for a job id, only accepted
  results represent that job, so an unaccepted 9841 from any signer cannot
  displace it. Where nothing was accepted, every signer's latest claim for
  that job id is surfaced rather than one silently winning; Job Results are
  keyed by `(job id, signer)`, never by job id alone.
- **Control reduction is per perspective.** Each `a` coordinate is reduced
  separately and the results OR'd, so a Stop on one maintainer's coordinate
  never closes a Request made on another's. The OR is the permissive
  direction — a Request standing on any perspective the caller passes counts
  as coverage — which is why callers pass the perspectives explicitly rather
  than letting the coordinator-authored `a` tags on the run widen the set.
- **Only a confirmed maintainer's Request is accepted.**
  `wasCIServiceRequestedWhenRunStarted` takes the latest control regardless of
  author; ngit treats the confirmed-maintainer list as the acceptance policy,
  so a stranger's Request never yields coverage while their Stop still closes
  their own Requests. An operator may accept other requester pubkeys for
  running work; that is not maintainer direction.
- **Delegation requires the accepting quote.** `getCIJobTrustResolution` only
  checks that *a* Workflow Result exists; ngit additionally requires that
  result to quote the exact Job Result, which is what rule 5 actually says.
- **Evidence classification is a separate type** from the trust
  classification, so `NoKnownContext` — an absence — is not representable on
  an evidence item (the TypeScript `Exclude<>`, enforced by the compiler).

### WP2 implementation decisions

- **Domain normalization strips the port before the root dot.** The
  TypeScript `normalizedDomain` lowercases, removes one trailing dot and
  *then* splits on `:`, so `grasp.example.:443` normalizes to
  `grasp.example.` and never matches `grasp.example`. ngit drops the port
  first. Both spellings name the same DNS host, so the reference order only
  ever loses a true match; the reversal cannot make two distinct hosts equal.
  Everything else about the ladder is a verbatim port, including that an
  exact match beats a subdomain in either direction, that the label boundary
  comes from the leading `.` in the comparison, and that siblings are
  nothing. gitworkshop should be aligned with this ordering.
- **A Manual Trigger's pull-request context must match the run's exactly.**
  Rule 4 only says PR-context triggers must match the run's PR root; ngit
  compares the two `Option<EventId>`s, so a PR trigger never covers a
  push-context run and a push/ref trigger never covers a PR run. The NIP has
  a PR run's trigger carry the PR tags, so the stricter reading costs
  nothing, and a rejection is never a negative claim — the run simply keeps
  no run-scoped maintainer direction.
- **The quoted event's signature is verified here, not at the fetch.**
  Everything after the kind check reads the quoted event's tags as the
  requester's statement, so `validate_run_provenance` requires
  `Event::verify` first. Without it a coordinator could supply a doctored
  copy of a real maintainer request — same event id, `p` rewritten to address
  itself — and pass every remaining check. Verifying at this layer rather
  than in `QuotedEventFetcher` means the guarantee holds whatever the
  caller's source is, so a fetcher may return unverified and even unrequested
  events.
- **A standing Service Request has no run context to match.** Its validation
  checks the id, kind, shape, author, repository closure, coordinator and the
  requester hint only. Temporal coverage of a *particular* run remains the
  control-history reduction's job, which is where WP1 already enforces
  non-retroactivity.
- **An unavailable quote is not a rejected quote.** `ProvenanceOutcome`
  separates `validated`, `rejected` and `unavailable`. An unavailable quote —
  one nothing could retrieve — yields no evidence *and* makes coverage
  partial, so it surfaces as **Context incomplete** rather than as a
  finding against the run.
- **A NIP-05 lookup error is unsettled, not falsified.** ngit reuses
  `client::nip05_query` (the path that resolves `nostr://` URLs) behind the
  `Nip05Lookup` trait. That call cannot distinguish a reachable document that
  omits the name from a transport failure, so every error settles as `failed`
  → partial coverage. A document that resolves the local part to *another*
  pubkey is a settled non-match: no evidence, no partial.
- **Every lookup is bounded.** `nip05_query` has no deadline of its own, so
  `NetworkNip05Lookup` wraps it in a five-second timeout —
  gitworkshop's `IDENTITY_TIMEOUT_MS` — and an elapsed lookup becomes a
  failed one. That is what makes the trust doc's "a bounded
  identity-resolution failure counts as settled" true here. Candidates are
  resolved one at a time, so the wait is bounded by the number of distinct
  candidates, and successes are cached across commands.
- **Identity candidates use every GRASP clone URL.** `RepoRef::grasp_servers`
  additionally requires a matching relay entry, which is the right test for
  publishing infrastructure but too narrow for evidence. `domain::
  repository_grasp_domains` keeps the host (with port) of every clone URL
  `is_grasp_server_clone_url` accepts, matching gitworkshop's
  `graspServerDomains`.
- **NIP-05 TTLs: one hour for a success, five minutes for a failure.** The
  cache is one JSON file per address, named by the hex of the address, under
  `<ngit cache dir>/ci-nip05`. A missing, unreadable, malformed, foreign,
  expired or future-stamped entry is treated as absent, so a corrupt cache
  costs a lookup and never a wrong answer. The cache is optional at every
  call site (`Option<&Nip05Cache>`) and its directory and TTLs are
  injectable, so tests never touch the user's cache or the clock.
- **Cache-tier coverage is partial whenever there is a signer to describe.**
  The domain ladder is skipped there by definition. A view with no signers
  has nothing to leave unchecked, so it stays complete rather than
  displaying a caveat about evidence that does not exist.
- **Control history is filtered to the maintainer closure once.**
  `resolve` reduces `CiInputs::controls` to the controls naming a coordinate
  in the closure before anything reads them, so a Service Request for a
  *different* repository — even one a maintainer of this repository signed —
  introduces no coordinator, no relationship entry and no requester
  attribution here. The filtered history is what the context stores and what
  every per-run reduction later reads.
- **Provenance verdicts are read-only.** `validated_provenance`,
  `rejected_provenance` and `unavailable_provenance` are private with
  accessors, like the repository facts beside them: an id enters the
  validated set only by passing `validate_run_provenance`, so no caller can
  manufacture maintainer direction for a run.
- **Callers declare their own coverage.** `CiInputs::input_coverage` carries
  the settlement of the caller's relay queries into the context, so a failed
  relay makes the whole context partial even when every check this layer
  performs succeeds.
- **Loading is a caller-side state.** Both tiers are called with the inputs
  they need and always return a settled context;
  `CiTrustContext::loading()` is what a caller renders while its own queries
  are outstanding. That is how rule 8 is kept structurally: nothing can
  settle a signer as "No known context" while a query it depends on is
  unresolved.
- **Level 3 is a seam, not a stub.** Social evidence would be appended per
  signer in `resolve::assemble` and its inputs added to `CiInputs`; nothing
  else in the model changes. No placeholder types were added for it.

### WP3 implementation decisions

- **The fetch filter lives where the filters are built.** The design pointed
  at `fetching_with_account`; the repo-wide filters it ultimately sends are
  assembled in `client::get_fetch_filters`, so the consumed CI kinds are one
  further `#a` filter there (`client::get_filter_ci_events`). That places
  them in the `RelayFetchScope::Repository` set only, so CI events are
  requested from repository relays and never from announcement indexers or
  user relays, and `process_fetched_events` caches them like every other
  repository event.
- **A refused `--require-ci-trust` is output, not a failure to produce
  output.** `main`'s JSON error path replaces the document with
  `{"status":"error"}`, which would discard the runs that explain the
  refusal. `ci status` therefore emits its full document — the `ci` object,
  plus `status: "error"` and the reason — and exits non-zero through
  `output::finish_and_exit`. The gate is evaluated on the rolled-up current
  result: not `concluded`, not `success`, or a weakest run below the floor.
- **A PR's `#E` anchor is not always the event ngit threads from.** When a
  patch thread is upgraded to a PR, ngit keeps the patch root as the thread's
  identity while every later PR update — and every CI event — references the
  kind-1618 upgrade root. `ci status` therefore resolves the anchor through
  the same `utils::pr_upgrade_root` rule the push path uses, rather than
  reimplementing it, and reports it as `target.anchor` beside `target.pr`.
  For the same reason, an exact event id that names a revision root (a patch
  revision, or that upgrade root) maps to the thread it revises rather than
  being taken for a thread root. Without both, an upgraded thread's `#E`
  query would silently find nothing and report `state: "none"`.
- **`revision_matched` distinguishes "no CI" from "CI for something else".**
  It is true when the target has no CI events at all, or at least one run
  describes the current revision; it is false only when CI exists for the PR
  but every run supplied a different revision. A run with no `e` supplying
  event matches only when the PR's current revision is its root.
- **A live marker wins the target state.** Any current run that is running
  makes the target `running`, even beside a workflow that already concluded;
  `concluded` then rolls up the worst of the concluded runs, where
  `neutral`/`skipped` rank below `success` so they neither fail the rollup
  nor displace a real outcome. `stale` is the state where every current run
  has only an expired marker.
- **`attempt_of` is the attempt count for the run's `(coordinator,
  workflow)` on this target**, which is the position of the current attempt.
  The design's JSON shape named the field without defining it.
- **The integrity marker is never an error.** A commit ngit does not hold
  reports `commit_present: false` and leaves `workflow_hash_matches` null; a
  commit it holds without the workflow file at that path leaves the hash
  null as well. Only a blob that hashes to something other than the `w`
  value is `false`. It is printed on its own line, below the trust label, so
  it cannot be read as a trust level. Every `c` value is tried and peeled, so
  a publisher that ordered an annotated tag object before the commit is still
  resolved. The hash is taken over the **blob** at that commit, never over
  the working-tree file: `core.autocrlf` and clean/smudge filters make the
  checkout differ from the object, and the object is what the NIP signs.
- **Runs are read per target, controls repository-wide.** A run query is a
  `#E` (PR) or `#c` (commit) filter, but Service Requests/Stops carry
  neither, so they are read by the repository's own `a` coordinates and
  handed to `resolve`, which filters them to the maintainer closure.
- **The full tier fetches missing quotes from the repository relays plus the
  quote's own relay hint**, and verifies NIP-05 through the WP2 machinery
  and its TTL cache. Coverage is routinely `partial` in practice — a GRASP
  host that serves no NIP-05 document is an unsettled lookup, not a negative
  claim — so the caveat is expected rather than exceptional.
- **CI signers' kind-0 profiles are fetched by this command.** A coordinator
  or compute provider is not a repository contributor, so the repository
  fetch never asks for its profile and the signer-declared `nip05` route
  would be dead evidence. The full tier therefore issues one targeted
  kind-0 query for the run and control signers over the repository relays
  plus the user's default relay set, caches what it gets, and reads the
  declared `nip05` back from the cache. It is best effort: an unreachable
  profile is an absence of evidence, so a failure is not surfaced. The
  synthetic `_@<grasp-domain>` route needs no profile and is unaffected.
- **Caller-side coverage comes from the fetch report.** `FetchReport::
  state_per_relay` records every repository relay the fetch reached; a relay
  whose query errored never reaches the consolidated report. Any announced
  relay missing from it makes `CiInputs::input_coverage` partial, which is
  how rule 8's "failed relays settle as partial" holds for this command
  rather than the trust layer's local checks being mistaken for the whole
  picture.
- **JSON ids and pubkeys are bech32.** The design's `<pubkey>` placeholder
  left the encoding open; every other ngit `--json` surface emits `nevent`
  and `npub`, and consistency across ngit's own output beats matching the
  wire encoding of a tag value. Commit ids stay hex.
- **`--offline` is "no relay fetch and no NIP-05", not "no network".**
  Resolving a repository coordinate from a `nostr://` remote in nip05 form
  can still perform the NIP-05 HTTP lookup that names the maintainer, before
  the offline guard is reached. The flag's help says what it actually
  skips.
- **The stale state is seeded into the local cache in tests.** A relay
  refuses an already-expired event (NIP-40), and waiting for a live marker
  to expire would be the wall-clock sleep the harness forbids. `stale` is by
  definition a marker ngit fetched while it was live and still holds, so the
  test writes it with `client::save_event_in_local_cache` — the same call
  the fetch would have made — and asserts on what ngit reads back. Every
  other CI fixture is published to a relay listed on the announcement.

### WP4 implementation decisions

- **One projection module, three surfaces.** The target types, the run
  selection, the state machine, the integrity check, the cache reads and the
  JSON/human rendering moved out of `ci_status.rs` into
  `src/bin/ngit/ci_projection.rs`. `ci status`, `pr view` and (WP5) `pr merge`
  differ only in the `ProjectionRequest` they pass: which `Tier`, whether
  earlier revisions are collected, and the caller's own coverage. The module
  is bin-local because it renders (`crate::output`, the stdout-guarding
  `println!`) and reads the `--require-ci-trust` floor; nothing in it belongs
  in `ngit::ci`.
- **`Target::select` splits rather than filters.** It now returns the
  current runs, the runs of superseded revisions, and `revision_matched`,
  computed from *this target's* runs rather than the whole query result.
  That is what lets `pr list` group one batched `#E` query for every listed
  PR without a per-PR query, and it is why `revision_matched` stays correct
  when the run set describes several PRs.
- **Outdated attempts are collapsed within a revision, never across.**
  `latest_attempts` keys on `(coordinator, workflow)` alone, so collapsing
  the whole outdated set would hide one revision's result behind another's.
  They are grouped by supplying event, collapsed inside each group, and
  ordered newest superseded revision first.
- **Evidence assembly sees every run known for the target; state,
  conclusion and the rollup never do.** `CiInputs::runs` is the current
  attempts *plus* the latest attempt of every superseded revision, on all
  three surfaces, whether or not the surface reports the outdated ones. A
  run for an earlier revision is legitimate identity-level evidence about
  its signer — gitworkshop's `classifyCICoordinatorRelationships` is
  likewise given every known run — and a coordinator whose only maintainer
  direction is an earlier revision's manual trigger is
  `previously-requested` no matter which command is asking. Feeding only
  the current runs, as the first cut of `ci status` did, made the same
  signer `no-known-context` in `ci status` and `operationally-associated`
  in `pr view`. Display, `state`, `conclusion` and the trust rollup are
  computed from the current runs alone, so an earlier result never becomes
  the PR's outcome. WP5's `--require-ci-trust` gate inherits both halves:
  it reads the rollup, which is current-only, over classifications that saw
  the target's whole history.
- **`outdated` is a JSON array, present only where the surface groups by
  revision.** `ngit ci status` describes one revision and emits no `outdated`
  key at all; `ngit pr view` always emits the array, empty when the PR has
  one revision. Each entry is a run object plus `revision`, the `nevent` of
  the event that supplied it (`null` when the run named no supplying event),
  so an earlier result cannot be read as current even by a consumer that
  flattens the document.
- **`pr list` costs three cache reads for the whole table, not three per
  row.** The anchors of every listed PR go into a single `#E` query; the
  service controls and the quoted requests are one read each; one
  `resolve_cache_tier` labels every run; and each row's cell is that
  context's rollup over its own current runs. Resolving a PR's anchor still
  costs the per-thread cache read `get_open_or_draft_proposals` already
  performs on every `git fetch`, and a thread that cannot be read shows no
  CI rather than failing the listing.
- **"No CI" has one encoding.** A row with no current run reports
  `classification: null` and `coverage: null` — there is no signer to
  classify and nothing was left unchecked — which is exactly what a PR
  whose thread could not be read reports. Rolling up an empty set would
  instead have claimed `no-known-context` about a signer that does not
  exist.
- **`cancelled` is not a pass.** The glyph table names `✓` passing and `✗`
  failing without placing `cancelled`; nothing ran to completion, so it is
  grouped with the outcomes that ask for a look. `neutral` and `skipped`
  stay passes, consistent with the rollup that already refuses to let them
  displace a real outcome.
- **The column and its footer appear only when a listed PR has CI.** A
  repository with no CI gets no column of dashes and no legend. The footer
  is one line: it reads the glyphs and says what `?` means — the *weakest*
  signer behind a passing result has no known context, which is what the
  conservative rollup surfaces — and points at `ngit pr view` for the
  evidence. It is ngit's own wording, not one of the canonical
  gitworkshop strings.
- **The list path expresses no coverage caveat in human output.**
  Cache-tier coverage is partial for every row that has a signer at all
  (WP2), so a `Context incomplete` line would appear beside every row and
  distinguish nothing. Coverage is reported per row in JSON, and the
  detail surfaces — where the domain ladder actually runs, and where the
  caveat therefore means something — print it.
- **The list JSON carries fields, not the glyph.** Every row has a `ci`
  object — `state`, `conclusion`, `classification`, `trust_floor_met`,
  `coverage`, `revision_matched` — including rows with no CI, so a consumer
  never branches on a missing key and never parses a symbol. The glyph is
  derived from those fields and is unit-tested against them.
- **`pr view` prints no empty section.** The `ci` object is always in the
  JSON document; the human "Checks" section is printed only when the PR has
  a current or an outdated run. The section is otherwise `ci status`'s own
  rendering, including the integrity line and the `Context incomplete`
  caveat.

### WP5 implementation decisions

- **One shortfall, two consequences.** `CiReport::gate_failure` was split:
  `shortfall(floor)` returns the bare reason a current result does not meet a
  floor — not concluded, not `success`, or a weakest run below it — and the
  gate is that reason prefixed with the flag that demanded it. `pr merge`'s
  warning is the same call at the default floor. The two surfaces therefore
  cannot drift into disagreeing about what "below the floor" means, and the
  refusal wording `ci status` already emits is unchanged.
- **The unnamed floor is `operationally-associated`.** The design fixed the
  floor only for an explicit `--require-ci-trust`; the warning needs one
  without a flag. It shares `pr list`'s, as `DEFAULT_TRUST_FLOOR`, so a row
  that renders `✓` is never a merge that warns and `✓?` is never a merge that
  is silent. Both call sites read the constant.
- **A target CI was never asked about does not warn; one whose CI describes
  another revision does.** `state: "none"` covers both, and only the first is
  an absence of interest: warning there would fire on every merge in every
  repository without CI. The second is `revision_matched: false` — CI ran for
  this PR, just not for what is being merged — which is exactly what a
  maintainer must not have to infer from silence, so it warns in its own
  words. An explicit `--require-ci-trust` refuses either: a caller that
  demanded a floor asked for a result, and there is none. A *running* target
  also warns: it is the same "has not concluded" shortfall as `stale`, and
  merging while CI is in flight is the case the warning exists for.
- **An unsettled trust context fails the floor.** `shortfall` used to return
  the "meets the floor" answer when the rollup had no classification, so a
  `TrustResolution::Loading` would have passed the gate silently. Both tiers
  always settle today, and `loading` is a caller-side state (WP2), so this is
  unreachable rather than latent — but a floor is a claim about evidence, and
  the absence of a classification is evidence that has not settled, never
  evidence that met it.
- **The gate runs before anything the command changes.** It is evaluated
  immediately after the open/draft status check, ahead of the branch creation
  and the `git merge`, so a refusal leaves no local branch, no merge commit
  and no status event. Like `ci status`, the refusal is emitted as the
  command's document through `output::finish_and_exit(1)` rather than
  `main`'s error path, which would replace the runs that explain it with
  `{"status":"error"}`.
- **`ci_warning` is a field, always present.** The human output prints the
  caveat; the JSON document carries it as `ci_warning`, `null` when there is
  nothing to say, so a consumer never branches on a missing key — the same
  rule the `pr list` row follows. It is always `null` when
  `--require-ci-trust` was passed: there a shortfall is the refusal in
  `error`, never a warning. `status`/`action` are derived from the presence of
  `error` (`ok`/`merged` against `error`/`refused`), since a refusal is the
  one outcome with an error and no published `event`.
- **The summary is the current revision only.** `include_outdated` is false,
  as in `ci status`: a merge is a decision about the revision being merged,
  and earlier revisions are `pr view`'s business. The document therefore
  carries no `outdated` key, consistent with WP4's rule that it appears only
  where the surface groups by revision. The section itself is printed only
  when the PR has a result, as in `pr view`; a refusal prints its reason
  regardless.
- **No new `--offline`.** `pr merge` already has one, and the tier falls
  straight out of it: `Tier::Cache` offline, `Tier::Full` otherwise, with the
  caller-side coverage taken from the fetch report exactly as `ci status`
  takes it.

Fetching: the consumed CI kinds (9840–9844, 39842) are one further repository
`#a` filter in `client::get_filter_ci_events`, added to the repository-scope
set built by `client::get_fetch_filters`, so CI events land in the normal
local cache during the fetch every PR command already performs. That set is
also what the `git-remote-nostr` fetch sends, so a `git fetch` against a
nostr remote carries the CI filter too. That is intended — it is how a
repository's CI history stays current for `pr list` without a command of its
own — and its cost is one extra filter per repository relay.

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

Add a "Checks" section: current-revision runs as in `ci status` (full tier,
cache tier under `--offline`), older revisions collapsed as outdated,
delegated jobs listed with their per-job resolution. The JSON document
carries the same `ci` object as `ci status`, with the superseded revisions
under `outdated`.

### `ngit pr list`

Add a `CI` column computed from the cache tier:

```
✓   passing, trust floor met (operationally-associated or better)
✓?  passing, but only signers with no known context
✗   failing (any trust level — a failure is a prompt to look, not a verdict)
…   running    ~ stale    -   none
```

`cancelled` is not a pass: nothing ran to completion, so it renders `✗`.
`neutral` and `skipped` are passes, as in the rollup. One footer line
explains `?` and points at `ngit pr view`; column and footer appear only when
a listed PR has CI. No network beyond the shared fetch; no NIP-05 lookups
from the list path.

### `ngit pr merge <id>`

Print the Checks summary before merging. `--require-ci-trust` as above;
without it, warn (non-blocking) when the current result is failing,
unfinished (running or stale), below the floor, or describes only a
superseded revision. Both read the same shortfall, so a merge that warns is
exactly a merge `--require-ci-trust=operationally-associated` would have
refused. The JSON document carries the shared `ci` object and a `ci_warning`
field.

### JSON shape (all surfaces)

Event ids are bech32 `nevent`s and pubkeys are bech32 `npub`s, as on every
other ngit JSON surface. Commit ids stay hex: they are Git object ids.

```jsonc
"ci": {
  "state": "running|concluded|stale|none",
  "conclusion": "success|failure|...",        // when concluded
  "revision_matched": true,
  "coverage": "complete|partial",
  "runs": [{
    "workflow": ".ngit/act/workflows/ci.yml",
    "state": "running|concluded|stale",       // this run, not the rollup
    "conclusion": "success",
    "attempt_of": 1,
    "run_id": "<workflow-run-id>",
    "commit": "<commit-id>",
    "coordinator": "<npub>",
    "classification": "maintainer-directed",
    "evidence": [{ "kind": "maintainer-request", "classification": "...",
                    "summary": "...", "authors": ["<npub>"], "scope": "run" }],
    "integrity": { "commit_present": true, "workflow_hash_matches": true },
    "jobs": [{ "job": "build", "conclusion": "success",
               "provider": "<npub>", "classification": "..." }]
  }],
  // Only where the surface groups by revision (`pr view`): the runs of
  // superseded revisions, each a run object plus the `nevent` that supplied
  // it. Never counted in `state`, `conclusion` or the trust rollup.
  "outdated": [{ "revision": "<nevent>", "workflow": "...", "...": "..." }],
  // Only when an event was skipped: its id, kind and the shape rule it broke.
  "skipped": ["skipped kind-9842 event <id>: ..."]
}
```

`ngit pr view` embeds that object as `ci` in its own document. `ngit pr list`
does not — one row is a cell, not a report — and carries per-row fields
instead:

```jsonc
"ci": { "state": "concluded", "conclusion": "success",
        "classification": "maintainer-directed", "trust_floor_met": true,
        "coverage": "partial", "revision_matched": true }
```

A row with no current run reports `state: "none"` with `conclusion`,
`classification` and `coverage` all `null`: no signer to classify, nothing
left unchecked.

`ngit ci status` wraps that object in the document every ngit command emits —
`status`, `entity: "ci"`, and a `target` describing what was resolved:

```jsonc
"target": { "kind": "pr", "pr": "<nevent>", "anchor": "<nevent>",
            "revision": "<nevent>" }
"target": { "kind": "commit", "commit": "<commit-id>",
            "commit_ish": "HEAD", "queried": ["<commit-id>", "<tag-object-id>"] }
```

`anchor` is the kind-1618 event CI references in `E`, which differs from `pr`
only for a patch thread later upgraded to a PR. A refused
`--require-ci-trust` sets `status: "error"` and adds `error`, keeping the `ci`
object that explains the refusal.

`ngit pr merge` embeds the same object in its own document, beside
`ci_warning` — the non-blocking caveat as a field, `null` when there is none:

```jsonc
{ "status": "ok", "action": "merged", "entity": "pr", "id": "<nevent>",
  "event": "<nevent>", "ci": { "...": "..." }, "ci_warning": null }
```

A refusal is `status: "error"`, `action: "refused"`, an `error`, no `event`,
and `ci_warning: null`.

Integration tests assert on this JSON and exit codes, never on table text.

## Work packages

Each WP is one reviewable unit with its own tests. Build in order; WP2+ depend
on WP1.

- **WP1 — `src/lib/ci/` core** (`kinds`, `events`, `controls`, `trust`)
  *(done)*: parsing, run grouping, control reduction, classification,
  rollups, temporal rules. Pure unit tests ported from the semantics above
  (total-order tie-break, retroactivity, stop scoping, weakest rollup,
  progress expiry).
- **WP2 — provenance + domain + resolve tiers** *(done)*: quote validation,
  NIP-05 domain ladder with TTL cache, cache/full tiers, coverage states. Unit
  tests for the domain ladder (label boundaries, ports, trailing dots) and
  coverage.
- **WP3 — `ngit ci status`** *(done)*: target resolution (order above),
  fetch-filter extension in `get_fetch_filters`, human + JSON output,
  integrity check, `--require-ci-trust`, `--offline`. Integration tests
  (`tests/ci_status.rs`, fixtures from `test_harness::ci`): JSON and exit
  codes for commit-ish, `#prefix`, nevent, and full-hex targets, including
  the ambiguity error.
- **WP4 — `pr view` Checks section + `pr list` CI column** *(done)*: the
  shared projection in `src/bin/ngit/ci_projection.rs`, full-tier Checks with
  outdated grouping and per-job delegation labels, a cache-tier column with
  one footer line. Integration tests (`tests/pr_view_checks.rs`,
  `tests/pr_list_ci.rs`): the current/outdated split, a delegated job and a
  signer with no context, the cache tier under `--offline`, one signer
  classified identically by `pr view` and `ci status`, and a row per glyph
  state.
- **WP5 — `pr merge` gating** *(done)*: the Checks summary before the merge,
  the non-blocking warning at the default floor, and `--require-ci-trust`
  refusing before anything is changed. Integration tests
  (`tests/pr_merge_ci.rs`): the blocked matrix (failing, no known context,
  stale, no CI) each with the unflagged merge as a control and the refusal
  keeping the runs that explain it, the allowed matrix (control-history
  coverage and a validated manual trigger, each isolated from the other's
  route, plus the cache tier under `--offline`), and the warning path —
  failing, running, a superseded revision, and a PR with no CI that must not
  warn.
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
