# Publishing static sites

`ngit nsite publish` publishes an already-built directory as a NIP-5A static
website. Files are stored as content-addressed Blossom blobs and the complete
path mapping is signed as one replaceable Nostr manifest.

## Command API

Publish the active account's root site with explicit servers:

```sh
ngit nsite publish dist \
  --title "My site" \
  --description "Static files published from this repository" \
  --blossom-server https://blossom.example.com \
  --blossom-server https://mirror.example.com \
  --json
```

Omit `--blossom-server` to discover the active account's latest kind-10063
server list. Repeat `--relay` to extend repository and account relay defaults
for manifest discovery and publication.

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
  repositories.

When `--source` is absent, ngit records the selected repository's canonical
`nostr://` URL. Metadata, server hints, every path mapping, and the recommended
aggregate `x` tag are included in the manifest.

NIP-5A does not define a logo metadata tag. Include a conventional asset such
as `/favicon.ico` or `/favicon.svg` in the build directory; it is published in
the path manifest like every other site file. Copy lineage, manifest snapshots,
and upstream app-descriptor links are not created by this first publish API.

`--concurrency N` controls simultaneous Blossom presence checks and uploads
and defaults to four. It does not increase signer concurrency.

## Deployment boundary

The positional directory is resolved from the process's current working
directory. Pass the build output itself: `ngit nsite` does not run a framework
build, interpret ignore files, or scan a repository for publishable files.

Every regular file beneath the directory is included. Paths must be UTF-8.
Symlinks and other non-regular entries are rejected rather than followed, so a
site cannot accidentally depend on files outside the declared build output.
Each file is copied into an immutable temporary snapshot before network work;
the manifest hash and uploaded bytes therefore cannot diverge if the original
build directory changes during publication.

## Upload and signing behavior

Before uploading, ngit sends bounded parallel `HEAD /<sha256>` checks to every
selected Blossom server. Only missing blobs are uploaded, but every unique blob
must be confirmed on every server before the manifest is signed.

Missing hashes are grouped into BUD-11 kind-24242 authorization events of up
to twenty hashes each. Each authorization is scoped to the selected server
domains and reused across those servers. A deployment with hundreds of files
therefore does not require one remote-signer approval per file. Duplicate file
contents are uploaded once while retaining every path in the manifest.

Ngit queries the exact current manifest before uploads and checks it again
afterwards. If another publisher changed the site, ngit leaves the
content-addressed blobs in place and refuses to sign over the concurrent
manifest. Relay publication succeeds when at least one selected relay
acknowledges the signed event; per-relay outcomes are returned in JSON.

NIP-5A manifests can exceed the historical 65 KiB NIP-44 boundary. Ngit uses
rust-nostr's extended NIP-44 length-prefix implementation so large manifests
can be sent to a NIP-46 remote signer. The signer itself must also implement
the extended NIP-44 format.

## JSON result

`--json` writes one terminal object to stdout. A successful
`nsite.publish` result includes the manifest coordinate and event ID, author,
aggregate hash, file and unique-blob counts, previous event ID, selected
servers, summarized Blossom work, and relay acknowledgements. Runtime failures
use `ok: false` with a stable error code; progress and signer diagnostics remain
on stderr.

Blobs which were stored before a later failure are safe to reuse because their
identity is their SHA-256. Rerunning the same command confirms them with HEAD
and skips their upload.
