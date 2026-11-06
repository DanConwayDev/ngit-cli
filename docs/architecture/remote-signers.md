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
One muted, indented line per signer relay shows its connection and the current
request’s delivery outcome. Subscription details and request IDs appear only
with verbose output. Each report ends with an action: check the signer and its
connections to accepting relays, or resolve the listed relay failures when none
accepted the request. Unconfirmed delivery is not reported as rejection.
Request results belong to the specific request; an earlier successful handshake
is not evidence that a later signature request reached a relay.

Pairing and ordinary signer operations use the same reporting boundary.
Client-initiated pairing first waits for the signer connection response, then
requests the user's public key. An established session may also need a protocol
connection handshake before its requested signature or encryption operation.
These stages should remain distinguishable in the terminal.

NIP-46 events are ephemeral. If the signing app was disconnected when a request
was published, reconnecting it may not recover that request. Open the signing
app, check pending approvals and its connections to the listed signer relays,
then cancel and rerun the command if necessary. ngit does not resume commands
from an intermediate signing step or automatically repeat signing requests.

Monitored operations for the same signer run one at a time so their progress
and pending requests cannot be mixed. Each remote operation has a two-minute
deadline covering queueing, the connection handshake and the requested operation
together. A timeout exits with recovery
guidance; a signer refusal keeps its original cause. Pairing failures propagate
instead of silently restarting the wait or returning to the account menu.
Cancelling or failing the pairing menu also stops its pending listener.

The instrumented client under `src/lib/remote_signer/` is derived from the
pinned upstream Nostr Connect client. Its provenance documents the narrow
adaptation needed to expose request progress. Keep protocol behavior aligned
with upstream when updating it, and keep secret keys, pairing secrets and
request contents out of diagnostics.
