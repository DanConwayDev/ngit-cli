# Remote signer progress

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

The instrumented client under `src/lib/remote_signer/` is derived from the
pinned upstream Nostr Connect client. Its provenance documents the narrow
adaptation needed to expose request progress. Keep protocol behavior aligned
with upstream when updating it, and keep secret keys, pairing secrets and
request contents out of diagnostics.
