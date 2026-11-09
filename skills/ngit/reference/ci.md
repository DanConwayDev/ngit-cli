# Nostr CI

Read when checking whether CI ran, interpreting a result, or writing a workflow
that uses ngit. Guides: https://ngit.dev/ci (coordinators, results, trust,
secrets) and https://ngit.dev/ci/workflows/ (what the coordinator accepts,
refuses, and adds compared with GitHub Actions).

## Workflows

Nostr CI (ngit-ci) runs workflows from `.ngit/act/workflows/` with GitHub
Actions syntax in Linux containers. `.github/workflows/` is run only by GitHub
Actions on a mirror; the directories are independent, so a check that must run
in both systems needs a file in each. ngit-ci refuses macOS and Windows
`runs-on` labels and job-level `uses:` (reusable workflows); composite actions
in steps work in both systems.

Read the workflow at the commit under investigation and confirm that its
triggers and steps cover the check in question:

```bash
git show <COMMIT>:.ngit/act/workflows/<WORKFLOW>.yaml
```

Install `ngit` and `git-remote-nostr` inside an ngit-ci or GitHub Actions job
with the step-level composite action, which verifies downloads against a
checksum-pinned manifest:

```yaml
- uses: danconwaydev/setup-ngit@v3
  with:
    version: 3.0.0 # optional exact pin; the default `latest` resolves against the action's manifest, not the network
```

Source:
`nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/setup-ngit`
(GitHub mirror `DanConwayDev/setup-ngit`).

## Query a result

```bash
ngit ci status <COMMIT|'#prefix'|nevent> --json                    # first query refreshes relays; no target means HEAD
ngit ci status <target> --json --offline                           # later cache-only reads
ngit ci status <target> --require-ci-trust maintainer-directed --json   # exit non-zero unless green at this floor
```

Query the exact commit that introduced the change. A PR target reports only its
latest revision. Read:

- `ci.state`: pending, running, or concluded. `ci.conclusion` counts only once
  the state is concluded.
- `ci.conclusion`: success, failure, cancellation, or another outcome.
  `command_status: "ok"` means only that the query worked.
- `ci.runs[].workflow` and `ci.runs[].jobs`: which workflow and job passed or
  failed.
- `ci.runs[].integrity`: the commit is present locally and the workflow hash
  matches.
- `coverage` and each run's classification and evidence: how completely and why
  the result is trusted. Partial coverage is not success.

If no run appears, report that no matching Nostr CI event was found. Then check
that the workflow existed at that commit, its trigger matched, a coordinator
serves the repository, and the query refreshed the relays before concluding that
CI did not run.

Trust floors are `maintainer-directed` and `operationally-associated`.
`ngit pr merge --require-ci-trust <LEVEL>` applies the same gate to a merge when
the caller wants one.

## Extended CI support

CI status recognises workflows triggered by opening, reopening or closing
an issue, or by publishing a release. Each result identifies the exact issue
action or signed release publication (including version, channel and asset
references). Distinct actions/publications remain separate at the same Git
commit; a retry replaces only an attempt for the same context. Release
revision means the signed publication event, not a different Git commit.
This reads CI evidence; it does not create issues, publish releases or
independently verify release assets.

Status JSON exposes workflow `details` and job `execution`: dependency and
rerun bindings, steps, outputs, artifacts, allocation/environment selection,
skip reasons and termination/cancellation evidence. Empty and omitted outputs
remain distinct. Aggregate success does not assert that every job succeeded.

A rerun can retain original job results. ngit checks exact source/new quotes,
coordinator, repository, commit, workflow and context. `reused` and
`execution_run_id` identify retained executions. Missing source evidence
does not authorize reuse; multiple quoted instances of one job stay distinct.

`ngit ci rerun COORDINATOR RESULT [--job JOB]` accepts a completed native
Nix result ID or nevent. It creates a new attempt, preserving external
dependency bindings; `--job` reruns that job and its dependants. It does not
cancel an active run. Confirmed maintainership is required, and publication
is not coordinator acceptance. Use status `result` nevents as inputs.

Both `trigger` and `rerun` accept repeatable `--environment JOB=SHA256[,URL]`
and `--require JOB=PATH:OP:JSON`. Overrides require native jobs that exist
in the pinned workflow. Environment URLs must match the digest.
`--rerun-dependencies` conflicts with rerun; use trigger for a fresh graph.

Job progress (39846) is matched to exact provider/allocation/job references.
Completed job results appear before the workflow finishes. Terminal evidence
wins over live snapshots; expired or future snapshots are not current progress.

`ci status` shows workflows, jobs and steps, including reported outcomes,
step durations, artifact publication state, counts and output links. Small
direct-artifact lists and manifest links are shown at the owning job; long
lists are summarized with a pointer to inspection. Signed durations are
milliseconds; step start offsets are job-relative, never wall-clock timestamps.

When the exact local workflow blob matches the event's hash, `declared_jobs`
provides expected jobs and stable step IDs. Native step keys and explicit act
IDs can be matched; generated/nested act IDs and matrix expansion remain runtime
information. Missing entries are `not reported`, not inferred as queued,
skipped or failed. Conditions are not evaluated locally. Native live snapshots
can initially describe planned, untimed steps as `skipped`; text distinguishes
these from a timed skip. Unavailable or mismatched YAML falls back to reported
execution evidence. Native explicit-step jobs publish live progress; current
act and simple native run/build jobs do not promise equivalent live steps.

`ci inspect <ID> --json` inspects exact evidence. Explicit --kind or --author
filters support protocol diagnosis; use `ci service status` for discovery.

`ci inspect <ID>` accepts an exact workflow, job or service event ID or
nevent, including an expired event when explicitly selected. It follows exact
quoted result/request/allocation references one hop and includes the status
trust context when the event can be associated with a known execution.
Missing evidence is not replaced by another attempt.

The default JSON event is decoded: `id`, `kind`, `type`, `author`, `created_at`,
`content`, and an ordered `tags` array. Each tag has its original `tag`, a
readable `name`, and its complete `values` array. Repeated tags, empty values,
unknown tags and extension slots survive unchanged. The signature is verified
before decoding; `--raw --json` includes the original signed event instead.
Signature verification is not coordinator admission or provider authorization.

`--include-expired` exposes stale metadata during discovery; exact-ID inspection
always reports its lifetime. `--limit` bounds results per kind (default 100,
maximum 1000). `--offline` reads the cache. Relay failures remain visible in
JSON. Ephemeral secret updates are not expected in retained-event queries.

`ci inspect <ID> --artifacts` explicitly retrieves the selected event's artifact
manifests. Downloads are bounded to 16 MiB each, verified against the Blossom
SHA-256, and checked for schema version and declared file count. No archive is
extracted and no listed file is downloaded. Status and watch never expand a
manifest automatically.

`ci status <target> --watch` first shows the current tree, then one compact line
per distinct workflow/job event received. It has no timeout and remains open
across completed attempts until interrupted. Ctrl-C stops observation without
cancelling execution. A PR target is pinned to the revision resolved at startup;
restart observation to follow a new revision. Ordinary network/cleanup bounds
still apply. `--offline` and `--require-ci-trust` cannot be combined with watch.

With `--watch --json`, stdout is JSON Lines: each record contains `event` in the
same decoded representation as inspect, plus `signature_verified`. There is no
synthetic per-artifact event, aggregate summary document, or trailing success
document. Every distinct received update is emitted, including late updates and
renewals; identical relay copies are deduplicated by event ID. This stream is
observations, not the latest-status reducer or an authorization verdict.

Subscription starts before the cache snapshot. Events are observed after SDK
signature/filter checks but before duplicate/replacement reduction, using a
bounded channel with backpressure. Connection diagnostics and failures use
stderr; fatal errors exit nonzero. The stream cannot reconstruct intermediate
replaceable snapshots already lost before connection or during disconnection.
Ordinary `status --json` retains its single-document summary contract.

`ngit ci secrets COORDINATOR --set-file secrets.json` reads a JSON map of
unique secret names to string values. Alternatively use repeatable
`--set-from-env NAME` to read each value from that environment variable.
`--remove NAME` removes names; set and remove can be one atomic update.
Values are never accepted on argv or printed. Names, reserved namespaces,
overlap, value lengths and total payload size are validated.

The signer must be a confirmed maintainer with their own repository
announcement. The command refreshes the coordinator's signed kind-19843
advertisement, validates its live secret key and inboxes, encrypts with a fresh
sender key using NIP-44 v2, and binds plaintext author/timestamp to the signed
kind-29846 event. Publication requires acknowledgement from an advertised inbox;
repository relays also receive the encrypted event. An expired key during
signing is rejected. No secret values are written to ngit's cache.

JSON reports `secret-update-published` with names only. Relay acceptance is not
coordinator acceptance: inspect kind-39844 status for names, origin and accepted
update times. `WORKFLOW_SECRETS_DECRYPTION_BUNKER` can be provisioned through the
same channel; pairing, sanitizing and sealing remain coordinator responsibilities.

`ci service request` and `ci service stop` manage standing repository service.
The older `ci request` and `ci stop` commands remain compatible, but emit a
deprecation warning on stderr. Their signed events and JSON results are unchanged.

### Repository service status

`ngit ci service status` shows coordinator evidence for this repository,
separately from `ci status` execution results. Each row identifies the exact
repository perspective, coordinator, service claim and maintainer-request state.
A live kind-39844 acting claim requires a live kind-19843 advertisement. Expired,
withdrawn or unavailable evidence is labelled; absence never proves stopped
service or operator configuration. Requests are reduced with maintainer-wide
and author-local Stop rules and current confirmed maintainer authority.

Candidates come from matching kind-19844 request-readiness lists (`a` for an
exact repository, `p` for its root owner), also requiring a live advertisement.
Unrelated coordinator and provider advertisements are not listed. Readiness
is a discovery hint, not a promise of acceptance. A newer list that removes
this repository supersedes its previous readiness claim.

`--offline` reads cached evidence. JSON includes separate service/request
states, evidence event IDs, relay failures and `history_complete: false`.
Relays may prune expired, replaced or deleted evidence; this is an observed
history, not an authoritative archive. An acting claim does not prove a live
network connection or that a particular workflow is eligible.

Bare `ci inspect` requires a target. Use `ci inspect <ID>` for evidence;
explicit `--kind` or `--author` filters remain available for protocol diagnosis.
Use `ci service status` for repository discovery.

Status keeps unstarted jobs on one row with declared `needs`, conditional and
runtime-expansion markers. These are declarations, not scheduling decisions.
Active jobs show reported steps and unreported unconditional steps; unreported
conditional steps are counted together. A native untimed `skipped` placeholder
is not evidence that a conditional step finished. Once execution is reported,
its outcome and duration take precedence. Finished skipped steps are summarized
as a count; `ci inspect <ID> --json` retains individual outcomes and reasons.
Conditions and dependencies are retained in `declared_jobs` JSON without local
evaluation, matrix expansion or predicting a success path. Missing events never
turn into inferred queued or skipped states.

### Selecting execution resources and environments

Native `ci trigger` and `ci rerun` accept per-job overrides for that request;
they do not edit workflow YAML. JOB is the key under `jobs`, not its display
name. For example, request at least 8 GiB RAM and four vCPUs for `build`:

```bash
ngit ci trigger "$COORDINATOR" HEAD --workflow .ngit/nix/workflows/ci.yml \
  --require 'build=memory-mib:gte:8192' \
  --require 'build=vcpus:gte:4'
```

`--require JOB=PATH:OP:JSON` is repeatable. Resource paths are literal,
case-sensitive keys; `memory-mib` is MiB and `vcpus` is a count. Environment
paths use `env` followed by a JSON Pointer, for example:

```bash
--require 'build=env/configuration/guest/job-uid:eq:0'
--require 'build=env/configuration/guest/no-new-privileges:eq:false'
```

Operators are `eq`, `gte`, `lte` and `contains`. Bounds require integers;
`eq` and `contains` accept integers, booleans or strings. Preserve JSON quotes
for a string, for example `--require 'build=some-resource:eq:"value"'`.
Comparisons do not coerce types. JSON Pointer keys escape `/` as `~1` and `~`
as `~0`; the wire path escapes literal `:` as `%3A` and `%` as `%25`.

Each named path replaces its whole previous constraint for that job, including
bounds inherited from coordinator, workflow or job settings. Other paths and
jobs keep their requirements. To set both bounds, repeat the flag with `gte`
and `lte`. Duplicate job/path/operator triples are rejected. Conditions select
compatible offerings; they do not change guest configuration or bypass provider
approval, isolation or secret rules. An unmatched request is not proof that
resources have been allocated.

To select an exact installed environment:

```bash
--environment "build=$ENVIRONMENT_SHA256"
--environment "build=$ENVIRONMENT_SHA256,$MANIFEST_URL"
```

The hash is the 64-character lowercase SHA-256 of the environment manifest,
not an image name, Git commit or environment-variable name. The optional URL
must name that digest. Obtain pins from a provider offering (kind 39845 `x`
tag) or an existing job's environment evidence through `ci inspect <ID> --json`;
explicit `ci inspect --kind 39845 --author <PROVIDER> --json` is available for
provider diagnostics. No automatic provider/environment chooser exists yet.
Pins select installed environments, do not request image installation, and do
not remove other requirements. Omitted pins retain the normal selection.
