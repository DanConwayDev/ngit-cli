# Nostr CI

Use this reference when checking whether CI ran, interpreting a result,
diagnosing why a failure was or was not reported, or writing workflows that
run ngit inside a CI job.

## Two workflow directories, one syntax

Nostr CI (ngit-ci) executes workflows from `.ngit/act/workflows/` with
[act](https://github.com/nektos/act), using GitHub Actions-compatible syntax
in Linux containers. Repositories mirrored to GitHub may additionally keep
workflows under `.github/workflows/`, which only GitHub Actions runs. The
directories are independent: a workflow in one is never executed by the other
system, so shared checks must exist in both directories (usually as identical
files).

- Put Linux-only jobs in `.ngit/act/workflows/`; ngit-ci cannot serve macOS or
  Windows `runs-on` labels.
- Keep macOS and Windows jobs in `.github/workflows/` only.
- Job-level `uses:` (reusable workflows) is refused by ngit-ci; composite
  actions in steps work in both systems.

## Installing ngit in a CI job

To run `ngit` or push/fetch `nostr://` remotes inside a job, install both
binaries with the setup action. It works in GitHub Actions (Linux, macOS,
Windows runners) and in `.ngit/act/workflows/` jobs, and verifies every
download against a checksum-pinned manifest:

```yaml
- uses: danconwaydev/setup-ngit@v1        # installs ngit + git-remote-nostr
- uses: danconwaydev/setup-ngit@v1
  with:
    version: 3.0.0-rc.5                    # optional exact version pin
```

`latest` resolves against the manifest pinned at the action ref, not a network
lookup. Do not compile ngit from source in CI or pipe `install.sh` to bash in
a job; the action is faster and hash-verified. Source:
`nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/setup-ngit`
(GitHub mirror `DanConwayDev/setup-ngit`).

## Locate the workflow

Nostr CI workflow definitions live under `.ngit/act/workflows/`. Inspect that
directory before conventional provider-specific paths. Read the workflow at
the target commit when history matters:

```bash
git show <COMMIT>:.ngit/act/workflows/<WORKFLOW>.yaml
```

Confirm that its triggers cover the event in question and that its actual
steps run the expected checks. Do not infer CI coverage merely from a local
test command or another provider's workflow.

## Query an exact commit

Refresh Nostr events on the first query in a session:

```bash
ngit ci status <COMMIT-ISH> --json
```

Use the exact commit that introduced the change, not only the current `HEAD`.
Subsequent cache-only inspection may add `--offline`.

To make the command exit non-zero unless CI is green and meets the requested
trust floor, add the gate explicitly:

```bash
ngit ci status <COMMIT-ISH> --require-ci-trust maintainer-directed --json
```

Interpret the result carefully:

- Top-level `status: "ok"` means the query command succeeded; it does not mean
  CI passed.
- `ci.state` reports whether the run is pending, running, or concluded.
- `ci.conclusion` reports success, failure, cancellation, or another outcome.
- `ci.runs[].jobs` identifies the failing or passing job.
- `ci.runs[].workflow` names the workflow used for that run.
- `ci.runs[].integrity` shows whether the commit is present locally and the
  workflow hash matches.
- `coverage` and each run's classification/evidence describe how completely
  and why the result is trusted; partial coverage is not evidence of success.

If no run appears, report that no matching Nostr CI event was found. Then
verify that the workflow existed at that commit, its trigger matched, and the
query refreshed the repository relays before concluding that CI did not run.

A successful push does not enforce CI by itself. Before merging a pull
request, use `ngit merge <ID|nevent> --require-ci-trust maintainer-directed
--json` when a failing, unfinished, untrusted, or absent result must block the
merge.
