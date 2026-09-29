# User-facing errors

Errors should explain what ngit was trying to do, what the available evidence
shows, and a concrete next step. Identify the relevant repository source, Git
server, Nostr relay, account selection or file path. Label addresses by their
role: a reader should not need to recognize `wss://` as a Nostr relay.

Do not claim that something does not exist merely because a lookup failed.
Distinguish a completed search with no match, an incomplete search, a local
cache miss without a network search, and unusable returned data. Preserve
operational failures such as cache permissions and repository validation.
Advice must follow the evidence: retries can help connection failures, but
cannot correct an invalid address or grant permission to publish.

Do not print secret keys, passwords, or arbitrary invalid input. A parsed
repository address or event ID can be useful; an unvalidated string could be a
secret pasted into the wrong argument. Keep underlying causes available when
adding context. Do not suggest weakening authentication, bypassing repository
membership rules, or creating a new announcement to repair a lookup failure.

Keep the default human message brief: one failure sentence, the relevant
source or evidence, and one focused next step. Put inspection commands on
separate lines when that helps scanning. Do not append a checklist of every
possible cause or repeat source-correction advice after an offline-cache miss.
Keep relay groups and underlying causes even when shortening the surrounding
prose. Structured JSON evidence should remain intact.

## Shared reporting boundaries

| Failure family | Shared boundary | Reporting behavior |
| --- | --- | --- |
| Repository discovery | `repo_ref.rs`, `RepositoryLookup` | Capture the selected source before fetching. Retain the real cause of explicit remote/URL resolution failures. |
| Required repository reads | `repo_lookup.rs` | Translate only `MissingRepoAnnouncement`; preserve other failures. Accumulate exact announcement-search outcomes from fetch reports. |
| Offline reads | `RepositoryLookup` captured before a cache-only read | State that the repository is missing from local data and no relays were searched. |
| Issue, PR and comment lookup | CLI `id_resolver.rs` | Use the same missing-item guidance for full IDs and prefixes, point to listing or thread commands that refresh the available data, and show candidates for ambiguous IDs. |
| Account and key failures | `login/mod.rs`, `login/existing.rs`, CLI login, `signer.rs` | Share key recovery guidance across selection and login; explain rejected signatures without exposing secrets. |
| Relay searches and publication | `client.rs` | Explain unfinished searches in plain language; preserve relay failure reasons and do not imply a partially delivered publication had no effects. |
| Git object downloads | `fetch.rs` | Keep per-server and per-connection-method failures through the final error; distinguish no configured servers from failed attempts. |
| Local repositories and key files | `git/mod.rs`, CLI `read_secret_file` | Include the path, operation, and relevant access guidance while retaining the OS cause. |
| Cache storage | `client.rs` | Existing local-cache errors identify the path and writable Git directory requirement. Global-cache fallback explains the in-memory behavior and `NGIT_CACHE_DIR` recovery. |
| Publication files and repository permissions | Publication commands, repo commands | Keep existing contextual path errors, structured publication codes and command-specific membership guidance. Generic lookup errors must not overwrite these refusals. |

Optional cache probes in startup hints, skill discovery, private-relay discovery
and init's creation-state classification are deliberately different from a
required repository read. A miss may be expected there. Keep their fail-closed
privacy and authority decisions; attach a diagnosis when the operation actually
requires the repository. Interactive discovery must verify the requested
announcement, rather than infer success from unrelated fetched profile updates.

`ngit repo` is informational: when a selected address has no usable repository
announcement it retains its successful exit status and partial information,
with an explicit lookup diagnostic. Cached results accompanied by a failed
refresh include a warning. Commands requiring a usable repository still fail.

## JSON and testing

Ordinary repository errors add `category` and `details` to the existing error
shape. Categories are `repository_not_cached`, `repository_not_found` and
`repository_lookup_incomplete`. Details include the source, its value, the
repository address, whether a network search was attempted, and relay outcomes
(`not_found`, `returned`, or `failed`, with a reason for failures). For a related
address whose individual search history is unavailable, `network_searched` is
null; do not borrow the parent address's outcomes or claim it was never queried.
A completed empty search is evidence about the queried relays, not universal
absence.

Publication commands retain their version-2 error envelope, putting the same
category in `error.code` and the same evidence in `error.details`. Informational
repository results use a `lookup` object for failed lookups. Do not replace
existing command-specific envelopes merely to make errors look identical.

Tests should drive real failure paths and check evidence, exit status and side
effects. Avoid full output snapshots. Cover empty and unreachable relays, mixed
outcomes, offline operation, source selection and JSON rendering. Verify that
operational errors are not relabeled as missing data, failed writes publish
nothing where that is the contract, and diagnostics do not change authority or
privacy behavior. Keep these expectations when adding a new command.

## Review scope and limits

The September 2026 review traced required announcement reads across CLI
commands, the Git remote helper, interactive repository selection and shared
release/container/site publication. It also inspected the shared selection,
item lookup, signing, transport and filesystem boundaries listed above.
Internal parsing invariants and protocol assertions are not all user-facing
recovery cases; retain their technical detail under an appropriate operation
context instead of replacing every error string mechanically.

This review does not change which accounts may publish, automatic repository
selection priority, publication transactions, Git authentication mechanisms or
network retry policy. In particular, the existing libgit2 SSH preflight checks
for default key files; its message now describes that actual check, but support
for agent-only credentials through that preflight is a separate behavior issue.
