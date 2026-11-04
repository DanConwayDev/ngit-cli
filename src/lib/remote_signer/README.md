# Instrumented NIP-46 client

`mod.rs` derives from crates.io `nostr-connect` 0.45.2 `src/client.rs`.
Its MIT license is retained in LICENSE. Keeping the module in ngit's sources
also includes it in crates.io packages, where Cargo patches are not retained.

The protocol implementation is retained. Adaptations use public upstream error
constructors, the existing futures dependency, and a broadcast progress stream
for subscription dispatch, matching EOSE readiness, request dispatch, SDK per-relay send outcomes,
and matching responses. Progress contains no request parameters or decrypted
contents. Send outcomes describe relay acceptance, not signer receipt, and are
available when the SDK finishes sending to the configured relays.

Remove this copy when upstream exposes equivalent progress instrumentation.

URI-based pairing exposes its URI after at least one relay sends EOSE for the
response subscription. A successful SDK subscription send alone only queues the
request and is not treated as readiness. This observer does not delay existing
interactive pairing or change the NIP-46 handshake.
