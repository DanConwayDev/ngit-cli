# Progress output

Verbosity selects the amount of information. JSON mode and stderr terminal
detection select how that information is rendered. Progress remains on stderr;
`--json` normally produces one final JSON document on stdout.
`ci status --watch --json` instead streams result snapshots and changes on
stdout. Agents can read stderr incrementally through a pipe without waiting
for the command to finish.

| Mode | Progress on stderr |
| --- | --- |
| Normal terminal | Concise initially, expanding after five seconds where supported; live animation |
| Verbose terminal | Detailed immediately, with final diagnostics retained |
| JSON | Blossom emits JSON Lines; other components still emit plain state changes. No animations or cursor clearing, even in a PTY |
| JSON with verbose | Additional detail where supported; the same mixed stderr formats and no animations or cursor clearing |
| Redirected output | Append-only progress at the selected verbosity; no timed refreshes |
| Quiet | No nonessential progress; actionable failures remain available |

Retries and phase changes remain observable in logs without repeated unchanged
reports or countdown ticks. Verbose Git-transfer output retains one completion
per transfer phase. Blossom additionally reports individual placement phases and
throttled byte samples: at 10% steps or on the next byte event after 15 seconds.
This is not a timer-driven heartbeat during a stalled upload. JSON Blossom
records have `type: "progress"`, `operation: "blossom"`, `schema_version: 1` and
an `event` object. See [release publication](../releases.md) for the contract.
Failure details are retained without replaying prior progress.

The policy lives in `output_mode::ProgressPolicy`. `ProgressPolicy::new` accepts
explicit output mode, JSON mode and terminal capability so rendering decisions
can be tested without a PTY or process-global environment changes. Production
callers use `progress_policy()`. JSON mode is shared by the library and binary.

A relay fetch or publish report records the historical outcome of that operation;
a later disconnect does not change it. A pending signer report instead tracks
current response-listening readiness, because the command still needs a reply.
This distinction is independent of the output policy.

## JSON API review (2026-11-10)

This is a source review of the command model and output boundaries at
`8c9d0566`, after PR `01ae4c14`. The recommendations below are not implemented.
It covers the shared paths behind account, repository, PR, issue, sync,
publication, CI and update commands. It does not claim an exhaustive runtime
trace of every command or third-party subprocess. No changes to signing,
publication, authorization, timeout or retry semantics are proposed.

### Recommended contract

Keep ordinary command results on stdout and emit incremental JSON Lines on
stderr under `--json`. A command's final result and exit status remain the
completion authority. Progress completion for an individual operation does not
mean the whole command succeeded. Keep CI watch's existing stdout result stream
and preserve the Git remote-helper protocol entirely.

Use a shared library-level stderr emitter so library and binary callers obey
one contract. Preserve the shipped Blossom envelope and event names. Extend
it with operation-specific typed payloads rather than converting prose into
fields or making consumers parse `message`. Use distinct diagnostic records
for warnings, notices and failures, with stable codes and optional human text.
Give concurrent/repeated operations invocation-local identifiers; a filename
alone is not a universal operation identifier. Include attempt and phase where
retries or sequential work would otherwise be ambiguous. Operation identifiers
must not double as protocol IDs unless that is their documented meaning.

Serialize and flush complete records with coordinated writes. Do not let
concurrent tasks interleave fragments, and bound any buffering. Byte counts
are cumulative within an attempt; totals may be unknown. Throttle byte updates
and suppress unchanged status, but never suppress phase transitions to satisfy
a byte-report budget. Emit starts and completions for short signer operations,
not only the delayed diagnostics currently shown to humans. Time-based
heartbeats, if introduced later, must be labelled as observations of waiting,
not evidence of new network progress.

`--quiet` should suppress nonessential progress but retain actionable failures.
Required interaction data, such as an explicitly requested pairing URI, is not
routine progress and must remain available. JSON mode must continue to avoid
terminal input, animations and cursor control. Do not turn signer requests
into automated approval or change retry policy as part of rendering work.

### Findings and migration order

Paths below are relative to the repository root.

| Priority / surface | Current behavior and evidence | Recommended change |
| --- | --- | --- |
| 1. Shared output and errors | `src/lib/output_mode.rs` writes plain progress via `write_progress_line` and `TransientLine`. `src/bin/ngit/output.rs` redirects human `println!` output to stderr in JSON mode. `main.rs` also prints plain errors after final JSON errors. | Add typed stderr emission in the library; migrate diagnostics and duplicate human result summaries deliberately. Merely wrapping every `println!` in a JSON string would create parseable logs without a useful API. Preserve existing final result/error envelopes. |
| 1. Remote signing and approval | `src/lib/signer.rs` funnels signing, encryption and decryption through `signer_progress::run_remote_operation`. `src/lib/signer_progress.rs` already consumes `ConnectProgress` request, delivery, listener and response events, but renders prose with delayed expansion. | Emit operation started, request sent, response-listener state, retry, response received and operation completed/failed records immediately. Include method, operation description, correlation and bounded wait information. A relay acknowledgement does not prove the signer received or approved the request; a received response is not necessarily successful validation. |
| 1. Pairing and account persistence | `src/lib/login/fresh.rs::login_with_nostrconnect` intentionally emits the pairing URI on stderr before waiting, including under JSON. Credential-storage receipts and recovery diagnostics also use prose. | Emit a typed action-required pairing record and persistence diagnostics. Preserve the necessary pairing URI only for the explicit pairing flow; exclude credential values and signing/encryption payloads from generic progress. Keep pairing distinct from later public-key approval and saved-login completion. |
| 1. Relay reads and writes | `src/lib/client.rs::RelayProgressReporter` and `RelayLog` render plain deduplicated state transitions; final fetch outcomes and publication reports already carry richer data. Address resolution in `src/lib/git/nostr_url.rs` and repository-source reporting in `src/lib/repo_ref.rs` emit separate text. | Emit typed resolution, fetch and publication phase records with relay identity, outcome and counts. Use data before it is flattened into display strings. Keep per-operation historical acceptance separate from live signer-listener readiness. This benefits read commands as well as `init`, `send`, issue/PR changes, sync and publication. |
| 2. Git transfers and sync | `src/lib/fetch.rs` and `src/lib/push.rs` use shared policy but emit text for servers, attempts, phase completions and failures. `src/bin/ngit/sub_commands/sync.rs` adds orchestration messages. | Emit transfer starts, protocol fallback, attempt outcomes and completion; expose useful counters where available. Distinguish object transfer, repository-state publication and proposal/status publication. Convert these for direct JSON CLI operations without modifying Git's remote-helper stdout protocol. |
| 2. Downloads and self-update | `src/lib/release_download.rs` streams and hashes chunks without a progress callback. `src/lib/self_update.rs` uses it for archives, then verifies, extracts and installs. These waits are not covered by the new Blossom upload renderer. | Add a download observer with byte counts and optional total, plus verify/extract/install phases for update. Cover URL-backed release assets too. Download completion is not digest verification or successful installation. Avoid exposing credential-bearing URLs or query parameters in new diagnostics. |
| 2. CI operations awaiting confirmation | `src/bin/ngit/sub_commands/ci_control.rs::SecretConfirmationWatch::wait` prints a plain waiting message; its final JSON distinguishes accepted and pending. Trigger/cancel/rerun publication also uses shared signing and relay paths. | Emit delivered, awaiting confirmation, confirmed and confirmation-timeout records. Preserve pending as an unconfirmed outcome, not rejection. Do not invent execution progress for commands that return after publishing a request. Never include secret values. |
| 2. Subprocess output | `src/lib/git/remote_helper.rs::read_pipe` forwards captured Git chunks as text. `src/bin/ngit/sub_commands/apply.rs` inherits `git am` stderr. `src/lib/self_update.rs::install_cargo_with_command` directs Cargo stdout and stderr to ngit's stderr. | Capture and frame output as diagnostic/log records under JSON, identifying producer and stream. Handle partial lines, carriage returns, non-UTF-8 output, size limits and concurrent pipe draining. Retain child exit status separately. Do not interpret arbitrary child prose as typed ngit progress. |
| 3. Remaining warnings and notices | Startup version/skill hints, account recovery, deprecations and command-local warnings bypass the shared progress helper through direct writes. | Audit these paths after introducing the emitter; use stable diagnostic codes, severity and useful context. Avoid replaying human success summaries already represented by stdout results. Do not promise JSON-only stderr until this audit and subprocess coverage are complete. |
| Separate contract: CI watch | `src/bin/ngit/sub_commands/ci_watch.rs` emits semantic result deltas on stdout. Watch notices and `output::finish_error` still print prose; the latter hardcodes “CI watch stopped”. | Keep stdout delta semantics, identifiers and completion behavior. Make stderr notices/errors structured; generalize the shared error wording. Do not add a trailing stdout result or move watch deltas to stderr under a general progress refactor. |

### Boundaries that need explicit treatment

- CLI parsing happens before `output::set_json_mode` in `main.rs`; Clap usage
  errors and the early version path currently sit outside ordinary JSON output.
  Decide and document whether usage/help/version output belongs to the machine
  contract. An emitter alone cannot convert these paths.
- The hidden remote-helper entry point runs before JSON CLI setup. Git owns its
  stdin/stdout protocol. Do not infer JSON mode from captured output or propagate
  it into a child remote helper's protocol channel.
- Publication commands use their existing version-2 final error envelope, while
  ordinary commands use `command_status` with optional category/details. Keep
  these contracts intact; stderr consistency does not require a simultaneous
  redesign of stdout.
- Explicit secret export and pairing commands have intentional sensitive
  outputs. Review those destinations individually; do not serialize arbitrary
  signer events, unsigned event payloads or raw CLI arguments into progress.
- Fast local reads do not need manufactured progress. Convert their diagnostics
  and reuse shared network progress only when they actually perform that work.
- The global JSON help in `src/bin/ngit/cli.rs` still describes stderr as plain
  state changes; update it alongside the staged rollout. During migration,
  document mixed stderr honestly rather than claiming all lines are JSON.

### Suggested implementation slices and validation

1. Introduce the shared stderr record writer and diagnostic schema, preserving
   the Blossom v1 contract. Test record boundaries, flushing, escaping,
   concurrent emission and broken-pipe policy with injected writers.
2. Convert remote signer operations and explicit pairing. Exercise a controlled
   signer that pauses until the test observes a progress record; then complete,
   reject or time out. Assert that progress arrives before stdout completion,
   no terminal input is consumed, and credentials/payloads are absent.
3. Convert relay fetch/publish and repository resolution, then Git transfers.
   Reuse local harness relays/servers to test partial acceptance and failures.
   Assert typed evidence, not exact human output.
4. Add download/update and CI-confirmation progress; test partial transfers,
   retry counters, unknown totals and the distinction between delivered and
   confirmed. Keep progress tests independent of wall-clock sleeps.
5. Frame subprocess output and migrate remaining warnings/startup/error paths.
   Add a representative command-family matrix asserting every stderr line is
   JSON under `--json`, stdout still has its documented shape, quiet-mode
   behavior is preserved, and child output cannot deadlock or grow unbounded.
6. Update CLI help, architecture and agent-facing documentation to promise
   JSON-only stderr only once the covered execution paths satisfy that contract.
   Record any deliberate parse/help/version and external-process exceptions.

For all slices, test JSON with both verbosity levels, quiet mode and nonterminal
output. Use pure policy tests for terminal selection; integration tests remain
flag-driven and isolated through `test_harness`. The existing Git remote-helper
protocol tests and CI watch delta tests must continue to pass unchanged.
