# Publishing static sites

`ngit nsite publish` publishes an already-built directory as a NIP-5A static
website. Files are stored as content-addressed Blossom blobs and the complete
path mapping is signed as one replaceable Nostr manifest.

## Command API

Ngit reuses nsyte's `.nsite/config.json` when it exists in the current
directory. The supported project fields are `id`, `title`, `description`,
`source`, `fallback`, `servers`, and `relays`; the file is JSON, not YAML. This
makes the normal publication command a direct replacement for `nsyte deploy`
once the static assets have been built:

```sh
ngit nsite publish dist --json
```

Use `--config PATH` to select another JSON file or `--no-config` to ignore the
project file. Explicit command-line values take precedence over config values.
For repeatable `--blossom-server` and `--relay` options, specifying any values
on the command line replaces the corresponding config array. With neither an
option nor a configured server list, ngit discovers the active account's
kind-10063 Blossom server list.

Nsyte's profile, relay-list, server-list, and NIP-89 app-handler publication
options are not implemented yet. A true `publishProfile`, `publishRelayList`,
`publishServerList`, or `publishAppHandler` config value produces an
`unsupported_nsite_config_option` warning while the site itself is published.
Signer credentials in nsyte configuration are not imported; select one of
ngit's stored accounts or pass an established `nbunksec` through ngit's signer
options.

Publish the active account's root site with explicit servers:

```sh
ngit nsite publish dist \
  --title "My site" \
  --description "Static files published from this repository" \
  --blossom-server https://blossom.example.com \
  --blossom-server https://mirror.example.com \
  --json
```

Selected relays are used for manifest discovery and publication and are
included as `relay` hints in the manifest. Without configured or explicit
relays, ngit uses its existing repository and account relay selection.

Publish a named site with `--id` (or its `--name` alias):

```sh
ngit nsite publish dist --id docs --json
```

Named identifiers follow NIP-5A's canonical DNS-suffix form: one to thirteen
lowercase ASCII letters, digits, or hyphens, without a trailing hyphen. With no
identifier, the command publishes the account's root kind-15128 site. With an
identifier, it publishes kind 35128 with that `d` tag.

Available metadata is:

- `--title TEXT`;
- `--description TEXT` or `--description-file PATH`;
- `--source URL`, accepting `https://` archives/repositories and `nostr://`
  repositories;
- `--fallback SITE_PATH`, mapping an existing HTML file in the build output to
  `/404.html` for single-page applications and custom not-found handling.

When `--source` is absent, ngit records a public selected repository's
canonical `nostr://` URL. It omits the inferred source for a private repository
so the public manifest cannot disclose that repository. An explicit HTTPS
source cannot contain embedded credentials. Metadata, server hints, every path
mapping, and the recommended aggregate `x` tag are included in the manifest.

NIP-5A does not define a logo metadata tag. Include a conventional asset such
as `/favicon.ico` or `/favicon.svg` in the build directory; it is published in
the path manifest like every other site file. Copy lineage, manifest snapshots,
and upstream app-descriptor links are not created by this first publish API.

Fallback mapping reuses the selected file's immutable snapshot and Blossom
hash, so it does not upload another blob. It replaces a real `/404.html`
mapping when both are present. The configured path must exist in the captured
build output and resolve to `text/html`.

`--concurrency N` controls simultaneous Blossom presence checks and uploads
and defaults to four. It does not increase signer concurrency.

## Deployment boundary

The positional directory is resolved from the process's current working
directory. Pass the build output itself: `ngit nsite` does not run a framework
build, interpret ignore files, or scan a repository for publishable files.

Every regular file beneath the directory is included. Paths must be UTF-8 and
end in a filename extension as required by NIP-5A. Control characters and URL
delimiters which would make path or aggregate parsing ambiguous are rejected.
Symlinks and other non-regular entries are rejected rather than followed, so a
site cannot accidentally depend on files outside the declared build output.
Each file is copied into an immutable temporary snapshot before network work;
the manifest hash and uploaded bytes therefore cannot diverge if the original
build directory changes during publication. A standard web MIME database maps
filename extensions to Blossom metadata, including formats beyond ngit's
software-release MIME list. Unknown extensions remain publishable as
`application/octet-stream`, with a path-specific warning in human and JSON
output.

## Upload and signing behavior

Before uploading, ngit sends bounded parallel `HEAD /<sha256>` checks to every
selected Blossom server. A present response must report the snapshot's exact
content length and MIME type. BUD-01 `307` and `308` redirects are followed
only when each target retains the requested hash. Only missing blobs are
uploaded. Ngit attempts replication to every selected server but signs the
manifest once every unique blob has at least one confirmed copy. Incomplete
replication is reported with copy counts for each server.

The presence display has one row per server beneath the aggregate progress
bar. Each row distinguishes copies already stored, blobs needing upload,
responses whose size or MIME metadata differs, failed checks, and checks
skipped after a server became unavailable. Presence checks share the global
`--concurrency` pool. After three consecutive transient failures from one
server, ngit stops scheduling its remaining initial checks while continuing the
other servers; an ordinary `404` is an upload candidate and does not count as a
server failure. Initial-check diagnostics remain available in live progress and
JSON but do not produce a final replication warning: no storage operation was
attempted, so they do not prove a missing copy. Failed uploads and post-upload
verification still warn. Exact post-upload verification keeps its bounded
retries because it determines whether an attempted write is safe to publish.

Missing hashes are grouped into BUD-11 kind-24242 authorization events of up
to twenty hashes each. Each authorization is scoped to the selected server
domains and reused across those servers. Ngit sends the current URL-safe,
unpadded BUD-11 encoding first; a server which responds with `401` is retried
once with the legacy padded encoding using the same signed event. A batch is
authorized only when its bounded upload work is ready to begin, and its
expiration covers both remote signer latency and the maximum queued request
window. A deployment with hundreds of files therefore does not require one
remote-signer approval per file. Duplicate file contents are uploaded once
while retaining every path in the manifest; identical bytes inferred as
different MIME types are rejected because Blossom stores one MIME type per
hash.

Ngit queries the exact current manifest before uploads and checks it again
afterwards. If another publisher changed the site, ngit leaves the
content-addressed blobs in place and refuses to sign over the concurrent
manifest. Transient presence and upload failures use bounded retries, and an
accepted or uncertain upload must pass another exact length-and-MIME `HEAD`
check before publication can continue. Relay publication succeeds when at
least one selected relay acknowledges the signed event; per-relay outcomes are
returned in JSON.
Nsite manifests are account-scoped public events: private-repository and
repository-only routing never suppresses account write relays, and private
repository relays are excluded unless repeated explicitly with `--relay`.

NIP-5A manifests can exceed the historical 65 KiB NIP-44 boundary. Ngit uses
rust-nostr's extended NIP-44 length-prefix implementation so large manifests
can be sent to a NIP-46 remote signer. The signer itself must also implement
the extended NIP-44 format.

## JSON result

`--json` writes one terminal object to stdout. A successful
`nsite.publish` result includes the manifest coordinate and event ID, author,
aggregate hash, file and unique-blob counts, previous event ID, selected
servers and relays, loaded config path, configured fallback, summarized
Blossom work with per-blob/per-server outcomes, and relay acknowledgements.
Runtime failures use `ok: false` with a stable error code; progress and signer
diagnostics remain on stderr.

Blobs which were stored before a later failure are safe to reuse because their
identity is their SHA-256. Rerunning the same command confirms them with HEAD
and skips their upload. If the resulting manifest is also unchanged, ngit
reports `changed: false` and reuses its event without another signer prompt or
relay write. A changed manifest waits for a strictly later wall-clock second
than its predecessor before it is signed once.
