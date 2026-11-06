# Remote signer progress

## Pair without terminal input

Run `ngit account login --nostrconnect --profile phone` to start a fresh
connection. Add `--local` for a repository-specific selection and repeat
`--signer-relay wss://relay.example.com` to choose signer relays. Stored profiles
are still reactivated with `ngit account login phone`.

The command prints one `nostrconnect: nostrconnect://...` line on stderr after
at least one relay confirms the response subscription with EOSE. It then waits
for the signer to pair and provide the user's public key. An agent can read the
URI while the process is running and hand it to the intended signer without
writing to stdin. This explicit mode displays no QR code and suppresses terminal
prompts, including when interactive mode was inherited from the environment.

With `--json`, keep reading stderr for the URI and progress; stdout contains
only the final JSON result. Keep draining both streams until the process exits.
The URI contains a pairing secret and should be shared only with the intended
signer. Failed pairing leaves the previous login selected. A successful pairing
saves the connection using the normal profile and credential-storage policy.

## Delivery and recovery

Remote signing has two independent connections: ngit's connection to each
signer relay and the signing application's connection to those relays. A healthy
ngit connection does not establish that the signer is online. Likewise, a relay
accepting a NIP-46 request does not establish that the signer received it.

ngit reports progress on stderr, preserving stdout for command results and the
single final document produced by `--json`. When an operation takes longer than
five seconds, a bold waiting line keeps the operation and event type visible.
The initial signing line stays visible until its replacement is ready. One
muted, indented line per signer relay uses a slow green spinner when awaiting a
response, an amber spinner while connecting or preparing delivery, and a red
cross for a failed request, subscription or connection. The heading has no
spinner and keeps the requested signing operation visible throughout any
protocol handshake or public-key request. “Awaiting response” requires a connected relay, confirmed response
subscription and accepted request; queued subscriptions remain unconfirmed.
Subscription details and request IDs appear only with verbose output. Reports
update in place on resends and delivery changes. The action line shows `Press R`
when keyboard recovery is available, followed by `Resend in Ns (attempted X/3)`;
the count is delivery attempts for this request, not the number of events being
signed. Lowercase `r` still works. A muted final tip reminds users to check that their signer is
connected to the listed relays. Pairing and failures retain specific guidance.
Unconfirmed delivery is not reported as rejection.
Request results belong to the specific request; an earlier successful handshake
is not evidence that a later signature request reached a relay.

Pairing and ordinary signer operations use the same reporting boundary.
Client-initiated pairing first waits for the signer connection response, then
requests the user's public key. The pairing report reminds users to approve
sharing that key and check for another signer notification, even when scanning
the QR code took longer than five seconds. An established session may also need
a protocol connection handshake before its requested signature or encryption
operation.
These stages should remain distinguishable in the terminal.

NIP-46 events are ephemeral. If the signing app was disconnected when a request
was published, reconnecting it may not recover that request. Open the signing
app, check pending approvals and its connections to the listed signer relays.

An eligible pending request is sent at most three times automatically: once
immediately, again after two minutes without a reply, and once more after four
minutes. It times out two minutes after the third send, at six minutes. A signer
refusal or terminal error stops immediately. Each send carries the same logical
request and signing payload in a fresh encrypted relay event. The first valid
reply completes the request; duplicate approval prompts are possible.

After five seconds, a direct `ngit` invocation with a Unix terminal offers
single-key `r` or `R` recovery without requiring `-i` or Enter. A manual resend
starts a fresh six-minute window: that send is followed by up to two automatic
resends, then a final two-minute wait. Repeated manual resends can extend the
wait while the user troubleshoots; Ctrl+C exits and restores terminal input.

`git-remote-nostr` never reads recovery keys or advertises the shortcut. Git
commands, JSON output and headless operators still receive automatic retries.
Pairing menus and `--nostrconnect` do not acquire another keyboard reader, and
one-time pairing secrets are never resent. The terminal countdown updates in
place; JSON and redirected output report state changes, attempts and outcomes
without timed refreshes or cursor clearing. Verbose mode reveals details
immediately and retains completed diagnostics. See [progress output](progress-output.md)
for the shared rendering policy.

Monitored operations for the same signer run one at a time so their progress
and pending requests cannot be mixed. Queueing and initial pairing retain their
two-minute bounds. Each eligible logical request has its own retry window; an
established session may first need a connection handshake before its signing
request. These finite protocol stages can each take up to six minutes when
unattended. Only manual resending replenishes the automatic-send budget.
There is no command checkpoint or mid-command resumption framework.

A timeout exits with recovery guidance; a signer refusal keeps its original
cause. Pairing failures propagate instead of silently restarting the wait or
returning to the account menu. Cancelling or failing the pairing menu also stops
its pending listener.

The instrumented client under `src/lib/remote_signer/` is derived from the
pinned upstream Nostr Connect client. Its provenance documents the narrow
adaptation needed to expose request progress. Keep protocol behavior aligned
with upstream when updating it, and keep secret keys, pairing secrets and
request contents out of diagnostics.
