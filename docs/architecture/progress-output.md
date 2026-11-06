# Progress output

Verbosity selects the amount of information. JSON mode and stderr terminal
detection select how that information is rendered. Progress remains on stderr;
`--json` produces one final JSON document on stdout. Agents can read stderr
incrementally through a pipe without waiting for the command to finish.

| Mode | Progress on stderr |
| --- | --- |
| Normal terminal | Concise initially, expanding after five seconds where supported; live animation |
| Verbose terminal | Detailed immediately, with final diagnostics retained |
| JSON | Concise, plain state changes; no animations or cursor clearing, even in a PTY |
| JSON with verbose | Detailed, plain state changes; no animations or cursor clearing |
| Redirected output | Append-only progress at the selected verbosity; no timed refreshes |
| Quiet | No nonessential progress; actionable failures remain available |

Retries and phase changes remain observable in logs, while repeated unchanged
reports, countdown ticks and transfer-byte samples do not produce new lines.
Verbose Git-transfer output retains one completion per transfer phase. Blossom
logs report batch and retry transitions, adding individual placement phases in
verbose mode. Failure details are retained without replaying prior progress.

The policy lives in `output_mode::ProgressPolicy`. `ProgressPolicy::new` accepts
explicit output mode, JSON mode and terminal capability so rendering decisions
can be tested without a PTY or process-global environment changes. Production
callers use `progress_policy()`. JSON mode is shared by the library and binary.

A relay fetch or publish report records the historical outcome of that operation;
a later disconnect does not change it. A pending signer report instead tracks
current response-listening readiness, because the command still needs a reply.
This distinction is independent of the output policy.
