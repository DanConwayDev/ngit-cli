# Container Publishing CLI and JSON API

Status: proposed.

This document specifies how ngit validates and publishes OCI image layouts
using Blossom storage and kind-30624 Nostr repository events. It is the
contract for the human-facing CLI and machine-facing `--json` result.

ngit implements the
[ncontainer Container Repositories draft](https://gitworkshop.dev/alex%40gleasonator.com/relay.ngit.dev/ncontainer/tree/main/ncontainer.md),
maintained in the authoritative
`nostr://alex@gleasonator.com/relay.ngit.dev/ncontainer` repository. That draft
is normative for the core kind-30624 wire format and gateway behavior. ngit
also requires the repository-binding `a` tag specified below. That extension
is expected to move into ncontainer when its relay discovery catches up with
NIP-34 repository relays. This document specifies that extension together with
ngit's CLI, validation, publication, and JSON contracts.

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
| `["a", "30617:PUBKEY:IDENTIFIER"]` | exactly one | current NIP-34 Git repository coordinate |
| `["tag", TAG, SHA256]` | one or more | mutable tag to bare lowercase manifest digest |
| `["server", URL]` | one or more | HTTP(S) Blossom server-root hint |
| `["title", TEXT]` | zero or one | display title |
| `["description", TEXT]` | zero or one | description |
| `["source", URL]` | zero or one | absolute HTTP(S) source URL |

`TAG` matches `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`. `SHA256` is exactly 64
lowercase hexadecimal characters without an algorithm prefix. Ordinary updates
preserve event tags unknown to this ngit version.

The `a` tag is an ngit-required extension to the current ncontainer draft.
ngit accepts only kind-30624 events whose single `a` tag is a kind-30617
coordinate. It must equal the repository selected from the current Git
checkout. This makes the container state repository-scoped even though the
addressable event coordinate remains publisher plus `NAME`.

The server tags are hints rather than per-blob availability proofs. Gateways
may also use the publisher's Blossom server list.

## Command

```text
ngit container publish NAME \
  [--layout PATH] \
  [--manifest PATH | --no-manifest] \
  [--blossom-server URL ...] \
  [--relay URL ...] \
  [--title TEXT] [--description TEXT] [--source URL] [--replace]
```

Global account selectors and `--json` apply normally.

The command must run inside a Nostr Git repository. The active signer must be
a confirmed maintainer of the selected repository.

| Input | Contract |
| --- | --- |
| `NAME` | required lowercase repository-name component |
| `--layout PATH` | OCI image-layout directory; required unless supplied by the selected manifest entry |
| `--manifest PATH` | load an explicit repository-relative or absolute container manifest |
| `--no-manifest` | ignore the default `.ngit/containers.yaml`; conflicts with `--manifest` |
| `--blossom-server URL` | optional repeatable override; HTTP(S) roots are deduplicated in order |
| `--relay URL` | repeatable addition to the current repository's relays |
| `--title TEXT` | optional non-empty display title |
| `--description TEXT` | optional non-empty description |
| `--source URL` | optional absolute credential-free HTTP(S) URL |
| `--replace` | replace complete tag/server state instead of merging |

ngit gets the base relay set from the consolidated kind-30617 announcement for
the current repository. It does not add the active account's NIP-65 read or
write relays, nor ngit's configured default relays. An explicit relay extends
rather than replaces the repository relay set. At least one repository relay
is required.

When no Blossom override is supplied, ngit queries those repository relays for
the latest kind-10063 server-list event authored by the active publisher and
uses its ordered `server` tags. At least one repository relay must complete
discovery. A missing or invalid latest list fails before upload rather than
falling back to an older event. An explicit list bypasses this discovery.

## Container manifest

The default project configuration is `.ngit/containers.yaml`. If it exists,
ngit loads it for every container publication. An explicit `--manifest PATH`
loads that file instead; a missing explicit file is an error. A checked-in
manifest that does not define `NAME` also fails closed. `--no-manifest` opts
out, in which case `--layout` is required.

```yaml
schema: 1
publication:
  blossom_servers:
    - https://blossom.example.org
    - https://mirror.example.org
  relays:
    - wss://relay.example.org
containers:
  api:
    layout: artifacts/api
    title: Example API
    description: Published from CI
    source: https://example.org/api
  worker:
    layout: artifacts/worker
```

The schema accepts only `schema`, `publication`, and `containers` at the top
level. `schema` must be `1`; `containers` must contain at least one valid
repository name. `publication` accepts ordered `blossom_servers` and `relays`.
Each container entry accepts `layout`, `title`, `description`, and `source`.
Unknown and duplicate YAML fields are rejected. Server and relay URLs are
validated and deduplicated while preserving order.

Relative manifest paths and manifest `layout` values resolve from the Git
repository root, not the process's launch directory. An absolute layout is
retained. A container entry may omit `layout` when CI passes `--layout`.

CLI `--layout`, `--title`, `--description`, and `--source` values take
precedence over the entry. A non-empty CLI Blossom list replaces the manifest
list; CLI relays extend the manifest relays. `--replace`, signer selection, and
output mode cannot be stored in the manifest. Thus stable project inputs can
be reviewed in Git while destructive or identity-bearing choices stay visible
at execution time.

## OCI layout contract

`PATH/oci-layout` must be a regular JSON file declaring
`imageLayoutVersion: "1.0.0"`. `PATH/index.json` must be a regular JSON file
with `schemaVersion: 2` and at least one manifest descriptor carrying the
`org.opencontainers.image.ref.name` annotation.

This `index.json` is only the local OCI image-layout table of contents. ngit
does not download it from Nostr and does not upload it to Blossom. The remotely
synchronized index is the `tag` map in the latest kind-30624 event: ngit fetches
that event, merges the new layout's tags, and republishes the complete event
state. An OCI multi-platform image index referenced by the local layout is a
different, content-addressed JSON blob under `blobs/sha256`; it is uploaded like
every other reachable OCI blob.

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
- only the selected Blossom server hints are published;
- omitted description and source are removed;
- unknown event tags are removed;
- title becomes `--title` or `NAME`.

Repository identity—the signing pubkey, `NAME`, and NIP-34 `a` coordinate—never
changes during an update. A latest event bound to a different Git repository
fails closed rather than being merged or overwritten.

## Upload and publication ordering

The first Blossom server receives a BUD-02 upload for each snapshot. Remaining
servers receive mirror requests in supplied order. Every upload or mirror must
return a valid descriptor and succeed before ngit signs a repository event.

Before uploading, ngit queries each current repository relay independently for
the exact author, kind, and `d` identifier. At least one repository-relay query
must complete; failures from the others produce warnings. ngit chooses the
NIP-01 latest event across all successful responses, requires its `a` tag to
match the selected Git repository, and merges the new tags into that complete
event state.

ngit repeats the repository-relay query after all uploads. Again, at least one
relay must complete. If the latest event ID changed, ngit refuses to overwrite
the concurrent update. Uploaded blobs remain reusable.

After the second preflight, ngit applies NIP-01 replacement ordering, signs the
event, and sends it to the repository relays. Overall command success requires
at least one relay acknowledgement. Callers requiring complete replication
must inspect every per-relay result.

The two reads detect changes visible during the upload interval but do not
provide compare-and-swap semantics. A publisher that updates the address after
the second read can still race this event; NIP-01 replacement ordering chooses
the winner rather than merging both events. The v1 API assumes one active
publisher for a given signing pubkey and repository name. Deployments sharing
that identity must serialize publication outside ngit.

No finite query proves global latest state. A container event stored only
outside the current repository relay set is not visible. When moving a
repository between relays, retain an old relay in the repository announcement
or pass it with `--relay` until the current container event has been
republished to the new set.

Requiring one successful query is an availability tradeoff, not proof that the
response is complete. If the only relay holding the current event is offline
while another repository relay successfully returns no event, both preflights
can agree on an empty base. A subsequent publication can then omit old tags.
Replicate each container event to multiple repository relays and keep at least
one state-bearing relay reachable during CI publication.

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
    "manifest_path": "/workspace/project/.ngit/containers.yaml",
    "npub": "npub1...",
    "name": "npub1.../myimage",
    "naddr": "naddr1...",
    "event_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "git_repository": "30617:abcdef...:source-repository",
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
`event_id` is raw hexadecimal; `naddr` is the portable container repository
address; `git_repository` is the exact coordinate emitted in the `a` tag.
`manifest_path` is the resolved path of the loaded configuration, or `null`
when no manifest was loaded.

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
