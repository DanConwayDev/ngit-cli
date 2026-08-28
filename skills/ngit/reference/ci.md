# Nostr CI

Use this reference when checking whether CI ran, interpreting a result, or
diagnosing why a failure was or was not reported.

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
