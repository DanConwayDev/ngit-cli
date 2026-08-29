# Container Publishing CLI and JSON API

Status: proposed.

This document specifies how ngit validates and publishes OCI image layouts
using Blossom storage and kind-30624 Nostr repository events. It is the
contract for the human-facing CLI and machine-facing `--json` result.

ngit implements the
[ncontainer Container Repositories draft](https://gitworkshop.dev/alex%40gleasonator.com/relay.ngit.dev/ncontainer/tree/main/ncontainer.md),
maintained in the authoritative
`nostr://alex@gleasonator.com/relay.ngit.dev/ncontainer` repository. That draft
is normative for the kind-30624 wire format and gateway behavior; this document
specifies ngit's CLI, validation, publication, and JSON contracts.

For a task-oriented walkthrough, see [Publishing containers](containers.md).
The canonical command group is `ngit container`; `ngit oci` is a visible alias.

## Scope

The v1 API:

- validates all OCI blobs reachable from tagged roots in an image layout;
- uploads each reachable blob to an ordered set of Blossom servers;
- publishes or updates one signed container repository event;
- preserves existing repository state unless exact replacement is requested;
- exposes deterministic success output for automation.

It does not build images, implement the OCI registry push protocol, pull or
list repositories, run a gateway, negotiate Blossom payments, chunk layers, or
delete remote content. Gateways are read-only consumers of the resulting event
and blobs.

## Protocol model

A container repository is an addressable event of kind `30624`. Its coordinate
is the publisher pubkey plus a `d` tag containing one repository-name
component matching:

```text
[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*
```

The event content is empty. It contains:

| Tag | Cardinality | Meaning |
| --- | ---: | --- |
| `["d", NAME]` | exactly one | repository identifier |
| `["tag", TAG, SHA256]` | one or more | mutable tag to bare lowercase manifest digest |
| `["server", URL]` | one or more | HTTP(S) Blossom server-root hint |
| `["title", TEXT]` | zero or one | display title |
| `["description", TEXT]` | zero or one | description |
| `["source", URL]` | zero or one | absolute HTTP(S) source URL |

`TAG` matches `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`. `SHA256` is exactly 64
lowercase hexadecimal characters without an algorithm prefix. Ordinary updates
preserve event tags unknown to this ngit version.

The server tags are hints rather than per-blob availability proofs. Gateways
may also use the publisher's Blossom server list.

## Command

```text
ngit container publish NAME \
  --layout PATH \
  --blossom-server URL [--blossom-server URL ...] \
  [--relay URL ...] \
  [--title TEXT] [--description TEXT] [--source URL] [--replace]
```

Global account selectors and `--json` apply normally.

| Input | Contract |
| --- | --- |
| `NAME` | required lowercase repository-name component |
| `--layout PATH` | required OCI image-layout directory |
| `--blossom-server URL` | required and repeatable; HTTP(S) roots are deduplicated in order |
| `--relay URL` | repeatable addition to the active account's write relays |
| `--title TEXT` | optional non-empty display title |
| `--description TEXT` | optional non-empty description |
| `--source URL` | optional absolute credential-free HTTP(S) URL |
| `--replace` | replace complete tag/server state instead of merging |

If neither account write relays nor explicit relays exist, ngit uses its
configured default relay set. An explicit relay extends rather than replaces
account write relays.

## OCI layout contract

`PATH/oci-layout` must be a regular JSON file declaring
`imageLayoutVersion: "1.0.0"`. `PATH/index.json` must be a regular JSON file
with `schemaVersion: 2` and at least one manifest descriptor carrying the
`org.opencontainers.image.ref.name` annotation.

ngit supports these manifest media types:

- `application/vnd.oci.image.manifest.v1+json`;
- `application/vnd.oci.image.index.v1+json`;
- `application/vnd.docker.distribution.manifest.v2+json`;
- `application/vnd.docker.distribution.manifest.list.v2+json`.

Every reachable descriptor must use `sha256`, and its regular file at
`blobs/sha256/<hex>` must match the declared size and digest. Image manifests
must provide a config and may provide layers; indexes may provide child
manifests. Tagged root documents must declare a media type matching their
descriptor because the kind-30624 tag map carries only a digest.

Only graphs reachable from tagged index entries are uploaded. Manifest JSON is
limited to 4 MiB and traversal to four nested child levels. Blob hashing uses a
bounded buffer. Before upload, each blob is copied to a stable temporary
snapshot and checked again, requiring temporary disk roughly equal to the
largest blob rather than the complete image.

## Update semantics

Without `--replace`, ngit reads the latest repository event and merges:

- a tag in the new layout replaces the same tag name;
- old tag names absent from the layout remain;
- supplied Blossom servers are appended to retained server hints;
- omitted title, description, and source retain their previous values;
- unknown event tags are retained;
- a new repository title defaults to `NAME`.

With `--replace`:

- only tags found in the new layout are published;
- only supplied Blossom server hints are published;
- omitted description and source are removed;
- unknown event tags are removed;
- title becomes `--title` or `NAME`.

Repository identity—the signing pubkey and `NAME`—never changes during an
update.

## Upload and publication ordering

The first Blossom server receives a BUD-02 upload for each snapshot. Remaining
servers receive mirror requests in supplied order. Every upload or mirror must
return a valid descriptor and succeed before ngit signs a repository event.

Before uploading, ngit queries every selected relay for the exact author,
kind, and `d` identifier. Failure to query any relay aborts the operation. It
repeats that query after all uploads; if the latest event ID changed, ngit
refuses to overwrite the concurrent update. Uploaded blobs remain reusable.

After the second preflight, ngit applies NIP-01 replacement ordering, signs the
event, and sends it to the selected relays. Overall command success requires at
least one relay acknowledgement. Callers requiring complete replication must
inspect every per-relay result.

## JSON result

`--json` writes exactly one JSON document to stdout. Human diagnostics and
upload progress remain on stderr. A successful publication has this shape:

```json
{
  "format_version": 1,
  "ok": true,
  "command": "container.publish",
  "result": {
    "repository": "myimage",
    "npub": "npub1...",
    "name": "npub1.../myimage",
    "naddr": "naddr1...",
    "event_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "tags": [{ "name": "latest", "digest": "..." }],
    "updated_tags": [{ "name": "latest", "digest": "..." }],
    "blobs": [
      {
        "sha256": "...",
        "size": 123,
        "servers": [
          {
            "server": "https://blossom.example/",
            "operation": "upload",
            "status": "stored",
            "descriptor": {
              "url": "https://blossom.example/<sha256>",
              "sha256": "...",
              "size": 123,
              "type": "application/octet-stream",
              "uploaded": 1787961600
            },
            "message": null
          }
        ]
      }
    ],
    "blossom_servers": ["https://blossom.example/"],
    "relays": [{ "url": "wss://relay.example", "accepted": true }]
  }
}
```

`tags` is the final published tag map; `updated_tags` contains only tags read
from this layout. Each server outcome has operation `upload` or `mirror` and,
on successful command completion, status `stored` or `already_present`.
`event_id` is raw hexadecimal; `naddr` is the portable repository address.

Before a success document is installed, failures use ngit's generic nonzero
JSON error shape:

```json
{ "status": "error", "error": "context: cause" }
```

Uploaded blobs may exist after a failure. Failures after all uploads explicitly
identify them as reusable; an upload or mirror failure can leave only a partial
set and the generic JSON error does not enumerate it. Content addressing makes
a retry safe after resolving the reported relay, signer, layout, or Blossom
condition.

## Gateway pull API

Publish and pull are deliberately separate. After publication, an OCI client
can pull through a compatible gateway. [ncontainer.io](https://ncontainer.io)
is the example gateway used by ngit documentation:

```bash
docker pull ncontainer.io/<npub>/<repository>:<tag>
podman pull ncontainer.io/<npub>/<repository>:<tag>
```

The gateway resolves the publisher and repository event, maps the tag to its
manifest digest, walks the authorized descriptor graph, and serves or redirects
the verified blobs. ngit neither operates nor configures that gateway.
