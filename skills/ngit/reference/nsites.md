# Nsites — publish static sites

Part of the ngit skill. Read this before publishing an already-built website
with `ngit nsite` or diagnosing its Blossom uploads and NIP-5A manifest.

## Publish

Pass the build output directory, not the source tree. Ngit includes every
regular file, does not run a build or apply ignore files, and rejects symlinks,
unsafe paths, and filenames without extensions.

Ngit automatically reads nsyte's JSON `.nsite/config.json`, so an existing
nsyte project can normally replace `nsyte deploy dist` with:

```bash
ngit nsite publish dist --json
```

Supported config fields are `id`, `title`, `description`, `source`, `fallback`,
`servers`, and `relays`. Use `--config PATH` for a different JSON file or
`--no-config` to ignore it. Explicit CLI values win; supplying any repeatable
`--blossom-server` or `--relay` values replaces that config array. The format
is JSON, not YAML.

```bash
# Root kind-15128 site; discover servers from the account's kind-10063 event
ngit nsite publish dist --title "My site" --json

# Named kind-35128 site, using explicit Blossom servers and an extra relay
ngit nsite publish dist \
  --id docs \
  --description-file site-description.txt \
  --source "nostr://<npub>/<identifier>" \
  --blossom-server https://blossom.example.com \
  --blossom-server https://mirror.example.com \
  --relay wss://relay.example.com \
  --json
```

Use `--title`, either `--description` or `--description-file`, and `--source`
for manifest metadata. `--source` accepts `https://` or `nostr://`; omit it to
infer the selected public repository. Ngit does not infer a private repository
source into the public manifest. NIP-5A has no logo tag, so put a conventional
`favicon.ico` or `favicon.svg` in the build output instead.

`fallback` or `--fallback SITE_PATH` maps an existing HTML file to `/404.html`
without another Blossom upload. The path must exist in the build output and
have an HTML MIME type; it replaces an existing `/404.html` mapping. Configured
or explicit relays are used for discovery and publication and emitted as
manifest `relay` hints.

Omit `--blossom-server` to use the active account's latest kind-10063 server
list. Supplying the flag overrides discovery; repeat it for replication. The
default `--concurrency 4` is a global limit across presence checks and uploads,
not a per-server limit. Values from 1 through 64 are accepted.

Nsyte's profile, relay-list, server-list, and NIP-89 app-handler publication
options remain future work. Enabled `publishProfile`, `publishRelayList`,
`publishServerList`, or `publishAppHandler` values produce a warning while the
site is still published. Use ngit's account and signer options instead of any
nsyte signer field.

## Signers and CI

Use an existing account alias for an ordinary publication:

```bash
ngit --signer <alias> nsite publish dist --json
```

For unattended NIP-46 publication, reuse an established connection and keep
the secret out of process arguments:

```bash
ngit --nbunksec-file /run/secrets/publisher-nbunksec \
  nsite publish dist \
  --title "My site" \
  --description "Published by CI" \
  --json
```

Read `reference/accounts.md` before creating, exporting, or storing signer
credentials. Do not establish a fresh bunker pairing on each CI run.

## Publication guarantees

Ngit snapshots the directory before network work and deduplicates identical
content. It checks every blob on every selected server, signs BUD-11 upload
authorization in batches of at most twenty missing hashes, and signs the
manifest only after all placements are confirmed with matching size and MIME
type. A failed deployment therefore cannot replace the live manifest with one
that points at known-missing content.

Rerun the same command after a failure. Content-addressed blobs already stored
on a server are confirmed with `HEAD` and skipped, so continuation works at
whole-blob/server granularity. Blossom does not define resumable partial PUTs;
an interrupted individual blob must be sent again unless the server completed
it and the next presence check confirms it.

An unchanged deployment reuses the current manifest without another manifest
signature or relay write. A changed deployment waits for a strictly later
observed second before signing once, ensuring that the new replaceable event
wins without rapid relay writes or event-ID mining.

## Interpreting JSON

Use `--json` for automation. Check the terminal envelope's `ok` value, then
inspect:

- `result.changed` to distinguish publication from an unchanged no-op;
- `result.config_path`, `result.fallback`, and `result.relays` for resolved
  nsyte-compatible settings;
- `result.blossom.blobs[].servers[]` for each blob/server outcome;
- `result.publication.relays[]` for manifest acknowledgements;
- `warnings[]` for unknown MIME types and unsupported future config
  publications.

On a Blossom failure, inspect `error.details.blobs` and
`error.details.possible_orphan_blobs`, then rerun after correcting the server
or signer problem. At least one relay must acknowledge the manifest, but every
selected Blossom server must confirm every unique blob.
