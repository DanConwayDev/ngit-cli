# Instrumented NIP-46 client

`mod.rs` derives from crates.io `nostr-connect` 0.45.2 `src/client.rs`.
Its MIT license is retained in LICENSE. Keeping the module in ngit's sources
also includes it in crates.io packages, where Cargo patches are not retained.

Adaptations use public upstream error
constructors, the existing futures dependency, and a broadcast progress stream
for subscription dispatch, matching EOSE readiness, request dispatch, SDK per-relay send outcomes,
and matching responses. Progress contains no request parameters or decrypted
contents. Send outcomes describe relay acceptance, not signer receipt, and are
available when the SDK finishes sending to the configured relays.

Remove this copy when upstream exposes equivalent progress instrumentation
and bounded pending-request retries.

URI-based pairing exposes its URI after at least one relay sends EOSE for the
response subscription. A successful SDK subscription send alone only queues the
request and is not treated as readiness. This observer does not delay existing
interactive pairing or change the NIP-46 handshake.

Automatic and manual resends retain the logical request ID and parameters but encrypt and
sign a fresh transport event so relay deduplication cannot hide the resend.
Only the current pending attempt can be resent; stale input cannot repeat a
later operation. The existing response matcher accepts the first valid reply
from either delivery. Connect requests carrying one-time secrets are excluded.

Eligible logical requests use three automatic sends two minutes apart and time
out two minutes after the third. An explicit manual resend starts that window
afresh. One-time pairing secrets and inbound pairing keep their original
protection; terminal signer errors never trigger retries. Progress exposes only
attempt counts and deadlines, not request contents.
