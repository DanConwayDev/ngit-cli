# Publishing containers

`ngit container publish` turns an OCI image layout into a pullable Nostr
container repository. `ngit oci publish` is a visible alias for the same
command.

The command uses the container repository protocol implemented by
[ncontainer](https://ncontainer.io): OCI blobs and Blossom blobs are the same
SHA-256-addressed bytes, while a signed addressable event of kind `30624`
maps mutable image tags to manifest digests.

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

The repository name is one lowercase OCI name component. Image tags come from
the `org.opencontainers.image.ref.name` annotations in `index.json`; they are
not inferred from filenames or Git tags.

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
omitted optional metadata, and unknown tags.

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

ngit queries every publication relay before the first upload and repeats that
query after the uploads finish. If another publisher changes the same
repository during that window, ngit leaves the content-addressed blobs in
place for reuse and refuses to overwrite the newer event. Replacement events
use ngit's NIP-01 timestamp/ID ordering policy so rapid consecutive updates
remain deterministic.

## Choosing servers

At least one explicit `--blossom-server` is required. Multiple servers are
strongly recommended: a missing layer prevents the entire image from running.
The server must accept `application/octet-stream` uploads as large as the
image's largest layer. Paid upload negotiation and layer chunking are not
currently supported.

`--relay` extends the active account's write relays. When the account has no
write relays and none are supplied, ngit's configured default relay set is
used. Publication requires a successful preflight on every selected relay so
ngit cannot mistake an outage for an absent current repository event.
