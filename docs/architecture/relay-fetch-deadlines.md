# Relay fetch deadlines

Each fetch retains the existing 45-second maximum in production. Repository
history switches to at most seven further seconds once at least half of the
repository requests in the current discovery round succeed (rounded up).
Auxiliary requests use the overall round's 50% threshold, so an isolated
metadata request need not consume the full maximum after other work finishes.
The grace period can shorten the original deadline; it cannot extend it.

Empty indexer or profile responses cannot shorten repository-history fetches.
A later round starts new counts: completed bootstrap requests are not successes
for newly discovered repository relays. Required author-announcement discovery
retains its protection until the announcement is resolved.

Protecting repository history can mean waiting longer than the old global
threshold when metadata finishes first. The maximum and grace durations are
unchanged; GRASP weighting and reconciliation are outside this policy.

The progress display uses each fetch's deadline measured from its start.
Timeout errors report elapsed time, rather than describing a seven-second grace
period as though it were the entire operation. These are total fetch deadlines,
including connection setup, query rounds and processing, not inactivity timers.
A successful fetch can still transfer cached history before reporting that it
found no new events; this change does not add incremental synchronization.
