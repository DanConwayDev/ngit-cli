# Event cache recovery

Repository events live in the Git common directory's `nostr-cache.lmdb`;
account and discovery events also use the global cache. `NGIT_CACHE_DIR`
overrides the global directory, not whether persistence is enabled. Credentials
and Git configuration are separate from these databases.

## Failure and recovery

An LMDB query returning `Not found` means an index references an event record
that is absent. A normal query with no matching events succeeds with an empty
result. An inaccessible directory or database instead produces an open/I/O
error; deleting it would not correct permissions or disk exhaustion.

`src/lib/cache.rs` handles known corruption and format errors during open,
query, event-ID lookup, save and delete operations. The backend hides its
internal error type, so classification uses its public error category and a
narrow set of diagnostics, covered by tests against the pinned version.
The backend also replaces write errors with `Batched transaction failed`;
that error alone is insufficient to reset a cache. A failed write must be
corroborated by a read that reports corruption.

Recovery creates a sibling `nostr-cache.lmdb.recovered-*` directory and
atomically publishes its basename in `nostr-cache.lmdb.current`. The original
database remains untouched. The same scheme applies to the test global cache.
Old ngit versions that ignore the pointer cannot modify the replacement.

A sibling `.recovery-lock` coordinates normal database operations and recovery
across current ngit processes. Operations take shared locks; replacement takes
an exclusive lock and rechecks the current generation before creating one.
Clients with old handles adopt the replacement before their next operation.
Do not rename, unlink, or overwrite a live LMDB environment: other processes
and the backend's asynchronous ingester may still hold it open.

Each process may replace a given cache once and retry an operation once.
Repeated corruption stops with an error instead of repeatedly discarding data.
Lock waits have bounded deadlines. Operational failures retain their path and
error context; the existing in-memory fallback remains available when the
global cache cannot be opened.

Online repository fetches watch both cache generations. Replacement invalidates
cached IDs, timestamps and discovery assumptions, so fetch planning restarts
with a bounded retry count. Offline commands do not fetch implicitly: restoring
missing history requires an online command. Recovery cannot recover events
that are no longer available from any relay. Preserved old files can help with
that diagnosis, including events cached by older ngit before publication.

## What may have caused issue ef38b7f9

The reporter's database and previous ngit version are unavailable, so its
original corruption trigger is not established. The investigation found:

- The SQLite-to-LMDB switch (`5979e13c`) changed the filename from
  `nostr-cache.sqlite` to `nostr-cache.lmdb`. A leftover SQLite file is not
  opened as LMDB.
- `nostr-lmdb` 0.45.2 migrates older databases by building the kind index and
  updating the schema version. It does not rebuild every existing index.
  A real LMDB fixture with a legacy schema marker and a dangling author/tag
  index still returns the reported `Not found` after that migration.
- Older writers, such as 0.43.0, do not know the later kind index. Alternating
  writers with different index sets against the same files can leave stale
  indexes when an older writer deletes or replaces events. This is a plausible
  version-skew mechanism, not proof that the reporter downgraded ngit.
- The upstream [0.44.0 changes](https://github.com/nostrdevkit/nostr/pull/1010)
  consolidated deletion and removed separate read transactions during batched
  writes. Older versions had different consistency behavior. Current writes
  update records and indexes in one transaction, so ordinary process termination
  is not by itself evidence for a dangling index.
- 0.45.3 changes packaging only; it does not contain an index-recovery fix.

The recovery tests exercise stale indexes, incomplete legacy migration,
incompatible schema versions, invalid LMDB bytes, replacement writes,
permissions, repeated corruption and concurrent processes. Recovery addresses
the observed failure without attributing it to the reporter's Linux version or
claiming a proven upstream trigger.
