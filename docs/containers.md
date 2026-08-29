# Publishing containers

`ngit container publish` turns an OCI image layout into a pullable Nostr
container repository. `ngit oci publish` is a visible alias for the same
command.

The command implements the
[ncontainer Container Repositories draft](https://gitworkshop.dev/alex%40gleasonator.com/relay.ngit.dev/ncontainer/tree/main/ncontainer.md).
That draft is authoritative for the core kind-30624 wire format and gateway
behavior. ngit additionally requires an `a` tag binding each container event
to the current kind-30617 Git repository. This repository-relay extension is
expected to move into ncontainer in the future.

For the complete CLI, wire-event, update, and JSON contracts, see the
[container publishing API](container-api.md).

## Publish an image

First export an image as an OCI image layout. For example, with Podman:

```sh
podman push myimage oci:/tmp/myimage:latest
```

Then publish every tagged image in that layout:

```sh
ngit container publish myimage \
  --layout /tmp/myimage \
  --blossom-server https://blossom-one.example \
  --blossom-server https://blossom-two.example \
  --relay wss://relay.example \
  --description "Example service image" \
  --source https://example.com/myimage
```

Run this command inside the Nostr Git repository the image belongs to. The
active signer must be one of that repository's confirmed maintainers. ngit
places the selected repository coordinate in the container event's `a` tag.

The repository name is one lowercase OCI name component. Image tags come from
the `org.opencontainers.image.ref.name` annotations in `index.json`; they are
not inferred from filenames or Git tags.

That `index.json` is local OCI layout metadata only. ngit neither downloads nor
uploads it. ngit instead downloads the latest kind-30624 event, merges the new
layout's tag mappings into its complete tag map, and republishes the complete
event. A multi-platform OCI image index inside `blobs/sha256` is a different,
content-addressed blob and is uploaded normally.

The active ngit account signs both the BUD-02 Blossom authorizations and the
kind-30624 event. The first Blossom server receives each reachable blob and
the remaining servers mirror it. Every required upload and mirror must succeed
before ngit signs the repository event.

By default, publishing behaves like adding tags to a registry:

- tags in the new layout replace tags with the same names;
- older tags not present in the layout remain published;
- previous Blossom server hints are retained and the new servers are added;
- omitted title, description, source, and unknown future tags are retained.

Pass `--replace` when the layout and supplied metadata should become the
complete repository state. This removes older tags, older server hints,
omitted description/source metadata, and unknown tags. The title becomes the
explicit `--title` or defaults to the repository name.

After publication, pull through any gateway implementing the protocol:

```sh
docker pull ncontainer.io/<your-npub>/myimage:latest
```

Use `--json` for a stable result containing the event ID, `naddr`, final tag
map, uploaded blob outcomes, Blossom servers, and per-relay acknowledgements.

## Validation and update safety

Before uploading, ngit verifies:

- `oci-layout` declares image-layout version `1.0.0`;
- `index.json` uses schema version 2 and contains at least one tagged image;
- repository names, image tags, media types, and SHA-256 digests follow the
  container protocol;
- every manifest, config, and layer reachable from a published tag exists and
  matches its declared size and digest;
- tagged root manifests declare their own media type, since kind-30624 tag
  entries carry only a digest.

Only reachable blobs are uploaded. Layers are hashed and streamed through a
bounded buffer rather than read into memory in full. Each blob is copied to a
stable temporary snapshot before upload so a concurrent layout mutation cannot
change the bytes after validation; allow temporary disk space roughly equal to
the largest blob.

ngit queries the current Git repository's relays before the first upload. At
least one relay must complete; other failures produce warnings. It uses the
NIP-01 latest response, requires the container event's `a` tag to match the
current repository, and merges the new tags into that event.

The query is repeated after the uploads, again requiring at least one
repository relay. If the latest visible container event changes during that
window, ngit leaves the content-addressed blobs in place for reuse and refuses
to overwrite it. Replacement events use ngit's NIP-01 timestamp/ID ordering
policy so rapid consecutive updates remain deterministic.

This is best-effort race detection, not a conditional relay write. Nostr
relays provide no compare-and-swap for addressable events, so an update made
after ngit's second query can still race its publication and NIP-01 will select
one replacement. Container repositories are intended to have one active
publisher at a time; coordinate publication externally when several processes
share the same signing identity and repository name.

No finite relay set proves global latest state. An event stored only outside
the current repository relay set is invisible to this preflight. Retain an old
relay in the repository announcement, or pass it with `--relay`, while moving
the repository between relay sets. Ordinary publication preserves every tag
from the latest event ngit finds; `--replace` remains the only intentional
complete-state replacement mode.

The at-least-one policy favors CI availability. If the sole relay holding the
current event is down while another repository relay answers with no event,
ngit cannot distinguish that from a first publication and may omit old tags.
Replicate container events across repository relays and keep one state-bearing
relay reachable during publication.

## Choosing servers

An explicit `--blossom-server` list overrides discovery. When it is omitted,
ngit uses the ordered `server` tags from the latest kind-10063 Blossom server
list authored by the active publisher. It discovers this list through the Git
repository relays. Discovery must complete on at least one repository relay
and fails before upload when no valid list is found.

Multiple servers are strongly recommended: a missing layer prevents the
entire image from running. The first server receives each upload and the
remaining servers mirror it in order. Every selected server is required. A
server must accept `application/octet-stream` uploads as large as the image's
largest layer. Paid upload negotiation and layer chunking are not currently
supported.

`--relay` extends the relays from the current repository announcement. Account
read/write and configured default relays are not added. Both preflights require
at least one successful repository-relay query; an individual relay outage
does not block publication.
