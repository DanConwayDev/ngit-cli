# Containers — publish OCI images

Part of the ngit skill. Read this before publishing an OCI image, updating a
container tag, choosing Blossom storage, using `ngit container`/`ngit oci`, or
constructing a gateway pull reference.

## Model

`ngit container publish` uploads the OCI blobs reachable from tagged entries in
an OCI image layout, then signs a kind-30624 addressable event mapping tags to
manifest digests. The event has an `a` tag binding it to the current kind-30617
Git repository. Blossom stores manifests, configs, and layers; Nostr owns the
mutable tag map.

Gateways are read-only. Publish with ngit, then pull through a compatible OCI
Distribution gateway such as `ncontainer.io`:

```bash
docker pull ncontainer.io/<npub>/<repository>:<tag>
```

## Publish

Export an OCI image layout first. For example:

```bash
podman push myimage oci:/tmp/myimage:latest
```

Publish every tagged root in the layout:

```bash
ngit container publish myimage \
  --layout /tmp/myimage \
  --blossom-server https://blossom-one.example \
  --blossom-server https://blossom-two.example \
  --relay wss://relay.example \
  --source https://example.com/myimage \
  --json
```

`ngit oci publish` is a visible alias. Use the active ngit account or a global
signer selector such as `--signer`; do not expose an nsec when a stored account
is available.

Run the command inside the Nostr Git repository the container belongs to. The
active signer must be a confirmed repository maintainer. The current
ncontainer draft defines the core kind-30624 event; ngit requires the NIP-34
`a` binding as an extension pending ncontainer's repository-relay update.

Important behavior:

- `NAME` must be one lowercase OCI repository-name component.
- Image tags come from `org.opencontainers.image.ref.name` annotations in
  `index.json`; filenames and Git tags are irrelevant.
- The layout's `index.json` is local input only; it is never downloaded or
  uploaded. ngit fetches and merges the complete tag map from the latest
  kind-30624 event. A reachable multi-platform image index in `blobs/sha256`
  is a different content-addressed blob and is uploaded.
- Without `--blossom-server`, ngit uses the active publisher's latest
  kind-10063 Blossom server list. An explicit ordered list overrides discovery.
  Two or more servers are strongly recommended. Every requested upload and
  mirror must succeed before ngit signs the repository event.
- `--relay` extends the current Git repository's relays. Account read/write and
  configured default relays are not added.
- ngit reads the repository relays before and after uploading. Each preflight
  needs at least one successful relay; individual failures only warn. Treat a
  total preflight or concurrent-update refusal as safe to retry: uploaded blobs
  are content-addressed and reusable. Retain or explicitly add a known old
  repository relay when changing relay sets; no finite set proves global
  latest state.
- At-least-one favors CI availability: a healthy empty relay cannot reveal an
  event stranded on an offline relay, so a publish can omit old tags in that
  case. Replicate container events and keep one state-bearing repository relay
  reachable.

## Merge versus replacement

Ordinary publication updates tag names found in the new layout while retaining
older tag names, previous server hints, omitted metadata, and unknown future
event tags.

`--replace` publishes only the new layout's tags and selected Blossom servers,
drops old description/source and unknown tags when omitted, and sets the title
to `--title` or `NAME`. Because it can remove published tags and metadata, use
`--replace` only when the user explicitly wants complete replacement.

## Machine output

Always use `--json`. A successful result has `command: "container.publish"`
and includes:

- `result.repository`, `git_repository`, `npub`, `name`, and `naddr`;
- raw-hex `event_id` (unlike collaboration commands' `id` fields);
- `tags` for the final repository and `updated_tags` from this layout;
- each blob's SHA-256, size, and per-server upload/mirror outcomes;
- final `blossom_servers` and per-relay `accepted` acknowledgements.

Do not infer complete relay replication from command success: publication
succeeds when at least one selected relay accepts the event. Inspect every
`result.relays[].accepted` value when full fanout matters.

## Limits

ngit accepts OCI and Docker v2 image manifests/indexes using SHA-256. It uploads
only blobs reachable from tagged roots and rejects missing, oversized, nested
too deeply, size-mismatched, or hash-mismatched graphs. It snapshots one blob
at a time, so allow temporary disk roughly equal to the largest layer.

The command does not build images, implement registry push, run a gateway,
negotiate paid uploads, chunk layers, pull images, list repositories, or delete
remote blobs/tags. Software release assets use the separate `ngit release`
event model; do not substitute release publication for a pullable OCI
repository.
