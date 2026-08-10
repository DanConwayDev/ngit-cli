# Software Release CLI and JSON API

Status: proposed.

This document specifies how ngit discovers, displays, and publishes software
applications, releases, and release assets using the NIP-82 event model. It is
the contract for both the human-facing CLI and the machine-facing `--json`
interface.

For task-oriented examples, including `.ngit/release.yaml` and ngit-ci
artifacts, see [Publishing releases](releases.md).

The canonical command group is `ngit release`, matching ngit's singular
`ngit pr` and `ngit issue` groups. `ngit releases` should be accepted as an
alias.

## Design inputs

- The local NIP-82 draft is the wire-format source of truth.
- GitWorkshop's factories, casts, discovery hooks, and release views establish
  the current interoperability baseline, including application-author trust
  and release platform aggregation.
- Zapstore's `zsp` CLI is useful prior art for source acquisition and APK
  enrichment: it extracts package/version, architecture, SDK, certificate,
  hash, size, and icon metadata instead of asking users to type everything.
  Its current event output is not treated as a wire-format oracle where it
  differs from the NIP-82 draft, such as a release without the required
  application `a` tag.
- ngit-ci's existing Blossom artifact uploads establish the local-file and
  authenticated-upload baseline.
- The pending rust-nostr Blossom work informs protocol handling, but ngit does
  not vendor or depend on that unreleased branch. The release client implements
  only the small BUD surface it needs with ngit's existing Nostr types.

## Goals

- List releases linked to the current repository.
- View a release and the complete metadata for every referenced asset.
- List applications linked to the current repository.
- Find applications owned by the current user, including applications not
  linked to the repository.
- Create applications and releases without accidentally replacing existing
  addressable events.
- Explicitly edit an application or release when replacement is intended.
- Add a new or existing asset to an existing release safely.
- Link an existing application owned by a repository maintainer to the current
  repository.
- Explain the distinction between repository maintainership and application
  ownership before a user attempts an unauthorized publication.
- Strongly encourage useful optional metadata, especially target platforms,
  without changing the NIP-82 wire format.
- Provide deterministic, scriptable JSON for every read and write operation.
- Publish assets already available at HTTP(S) URLs or upload stable local-file
  snapshots to ordered Blossom servers.

## Non-goals for v1

- Installing, updating, or executing release assets.
- Dependency resolution, update-channel policy, or release signing beyond
  Nostr event signatures and asset hashes.
- Acting as a global application catalogue or silently publishing to
  Zapstore. The explicit `--zapstore-relay` shortcut is additive and never
  changes Blossom storage.
- Editing immutable kind `3063` asset events. Corrections require a new asset
  event and an explicit edit of the release which references it.
- Silently adopting applications published by non-maintainers.
- Blossom payment negotiation, media optimization, deletion, and blob listing.
- Publishing mirror URLs in NIP-82 extension tags. v1 publishes the primary
  Blossom URL and reports mirrors through command output.

## Protocol model

The API consumes and emits the following NIP-82 event types:

| Kind | Entity | Replaceability | Identity |
| ---: | --- | --- | --- |
| `32267` | application | addressable | author plus `d` |
| `30063` | release | addressable | author plus `d` |
| `3063` | software asset | immutable regular event | event ID |

### Application

A software application event has:

- required `d` and `name` tags;
- optional description in event content;
- optional `summary`, `icon`, `image`, topic `t`, website `url`, repository,
  repository-coordinate `a`, platform `f`, and SPDX `license` tags.

ngit-created applications MUST include an `a` tag for every currently known
coordinate of the repository. The selected maintainer's coordinate MUST be
first, followed by the remaining coordinates in the ordering defined by
[the maintainer model](architecture/maintainer-model.md). A canonical clone URL
SHOULD be included in `repository` when one can be resolved.

### Release

A software release event has:

- an `a` tag identifying its kind `32267` application;
- `i` application identifier, `version`, and `c` channel tags;
- a `d` tag equal to `<application-id>@<version>`;
- one `e` tag for each referenced kind `3063` asset event;
- `f` tags equal to the deduplicated union of the referenced assets' `f` tags;
- an optional `commit` tag containing the full Git commit ID represented by the
  release;
- release notes in event content;
- `created_at` set to the release date when the release is first published.

ngit MUST derive the `d`, `i`, application `a`, asset `e`, and release `f` tags.
Callers MUST NOT be able to provide contradictory values for those tags.

### Asset

A software asset event has optional application provenance through an `a` tag
identifying its kind `32267` application, plus required `i`, `m`, `x`, and
`version` tags for the asset identifier, MIME type, SHA-256 hash, and asset
version. When present, the asset author MUST own the referenced application
coordinate. Legacy assets without the pointer remain valid, while ngit-created
assets always include it. An asset's identifier and version are independent of
the application and release values.
URL-backed assets also have a `url` tag. ngit SHOULD publish all metadata it can
establish, including:

- `filename` and byte `size`;
- one or more platform `f` tags;
- minimum and target platform versions;
- supported NIPs;
- build `variant` and source `commit`;
- minimum allowed asset version;
- Android version code, minimum allowed version code, and APK signing
  certificate SHA-256;
- the original web source in an `r` tag when it differs from the asset URL.

The URL, MIME type, hash, size, and filename MUST describe the same bytes. ngit
MUST stream the asset bytes itself to compute the hash and size; HTTP headers or
manifest claims are hints, not integrity evidence.

## Trust and publication authority

NIP-82 application ownership is narrower than repository maintainership. The
author of the application event owns its addressable coordinate. Only that
author can replace the application or publish authoritative releases and
assets for it.

An application is **trusted and linked** to the current repository when:

1. its author is in the repository's current directional maintainer graph; and
2. it contains an `a` tag for at least one current repository coordinate.

An arbitrary author cannot gain trust merely by adding the repository's
coordinate to an application.

The active signer has the following capabilities:

| State | View | Link application | Publish release |
| --- | --- | --- | --- |
| author + maintainer, linked | yes | already linked | yes |
| author + maintainer, unlinked | yes | yes | after linking |
| other maintainer, linked | yes | no | no |
| author is not a maintainer | explicit; untrusted | no | no |

Read commands MUST NOT require a signer unless the command needs the identity
implied by `--mine`. Write commands MUST resolve both the active signer and the
current maintainer graph before signing anything.

When a maintainer is not the application author, human output MUST explain the
reason before any asset download or event signing. For example:

```text
cannot publish ngit@1.8.0: the application is owned by
npub1alice...; repository maintainership does not grant release authority.
log in as npub1alice... to publish this release.
```

JSON output MUST expose the same condition through the `authority` object and a
stable blocker code. It is not sufficient to report a generic signature or
permission error after doing publication work.

## Selectors

Commands accept these selectors:

- `APP`: an application identifier, a kind `32267` `naddr`, or a raw
  `32267:<pubkey>:<identifier>` coordinate;
- `RELEASE`: `<application-id>@<version>`, a kind `30063` `naddr`, or an
  event ID/`nevent` for a specific observed revision;
- `ASSET`: a kind `3063` event ID or `nevent`.

A bare release version is accepted only with `--app APP`, or when exactly one
trusted linked application makes it unambiguous. A bare application identifier
which matches multiple authors is an error; ngit MUST show the candidate
coordinates and require an address-qualified selector. Asset filenames are
display labels, not globally safe selectors. `release asset view` therefore
requires an event ID unless the filename is qualified by `--release` and is
unique within that release.

Selectors MUST be parsed without normalizing application identifiers or
versions. In particular, `v1.2.0` and `1.2.0` are different Nostr addresses.

## Command summary

| Command | Purpose |
| --- | --- |
| `ngit release list` | list releases for trusted linked applications |
| `ngit release view RELEASE` | show a release and resolve all asset details |
| `ngit release publish VERSION` | create or explicitly edit a release |
| `ngit release app list` | list linked or user-owned applications |
| `ngit release app view APP` | show application details and authority |
| `ngit release app init` | create or explicitly edit an application |
| `ngit release app link APP --edit` | link an existing application |
| `ngit release asset list RELEASE` | list the assets referenced by a release |
| `ngit release asset view ASSET` | show complete asset metadata |
| `ngit release asset add RELEASE --edit` | attach asset and edit release |

`application` SHOULD be accepted as an alias for `app`.

Every leaf command MUST accept `--json`. Read commands MUST accept `--offline`.
Write commands are online-only in v1 because a safe overwrite preflight and
publication cannot be performed from a potentially incomplete cache.

## Common command behavior

### Repository resolution

Commands resolve the repository through ngit's normal repository-coordinate
rules, including `--repo`, `nostr.repo`, and `nostr://` remotes. They MUST use
the same maintainer graph and coordinate ordering as other ngit repository
commands.

Repository relays, the relevant authors' NIP-65 relays, ngit's configured
defaults, explicit `--relay` values, and the local cache form the discovery
set. `--zapstore-relay` explicitly adds `wss://relay.zapstore.dev` only to the
publication targets and their strict preflight query. It MUST NOT add Zapstore
to general discovery or Blossom kind-10063 server-list discovery. There MUST
NOT be a Zapstore-specific relay hidden in default behavior. Explicit relay
options extend the discovery set unless the existing global
`--repo-relay-only` behavior narrows it.

An online create or edit preflight MUST wait for end-of-stored-events from each
target publication relay and at least one discovery route for the author. The
guard can only cover the queried relay set; Nostr cannot prove that an event
does not exist on every relay in the world. The CLI MUST state this boundary in
verbose output and documentation.

### Create and edit guard

Application and release publication uses strict create/edit semantics:

| Existing addressable event | `--edit` | Result |
| --- | --- | --- |
| no | no | create |
| yes | no | refuse with `*_already_exists` |
| yes | yes | replace using patch semantics |
| no | yes | refuse with `edit_target_not_found` |

`--edit` MUST NOT behave as an upsert. A global `--force` MUST NOT be treated as
an alias for `--edit`.

All addressable identities are immutable during an edit. Editing an application
cannot change its author or `d` identifier. Editing a release cannot change its
author, application, or version. The user creates a new address when one of
those values changes.

Edits use patch semantics. Unspecified typed fields, unknown tags, and unknown
future tags MUST be preserved. ngit MUST reconstruct managed tags into a
canonical, deduplicated block so duplicate source tags cannot smuggle a second
value past a typed field. An explicit clearing option is required to remove an
optional value.

### JSON contract

With `--json`, stdout contains exactly one JSON object for every runtime success
or failure. Human tables, prompts, spinners, progress, and notices MUST NOT be
written to stdout. A runtime failure returns a non-zero status but still writes
its terminal JSON object to stdout. Argument parsing failures produced before
command dispatch retain clap's normal stderr behavior.

JSON objects have this common envelope:

```json
{
  "format_version": 1,
  "ok": true,
  "command": "release.view",
  "repository": {
    "selected_coordinate": "30617:...:ngit",
    "coordinates": ["30617:...:ngit"]
  },
  "authority": {
    "current_signer": "...",
    "application_author": "...",
    "is_current_maintainer": true,
    "application_linked": true,
    "can_publish": true,
    "blocker": null
  },
  "warnings": [],
  "result": {}
}
```

Fields which are not meaningful for a command are `null`; they are not omitted
from the common envelope. Entity objects contain parsed semantic fields and a
`raw_event` object. Lists MUST have deterministic ordering and MUST be arrays,
including when empty. Timestamps are integer Unix seconds. Byte sizes and other
potentially 64-bit counters MUST be decimal strings so JavaScript consumers do
not lose precision.

Entity `event_id` and `author` fields are canonical lowercase protocol hex.
Their adjacent `event_id_bech32` and `author_npub` fields provide explicit
user-facing encodings; callers MUST NOT infer the encoding from a field's
contents.

Runtime errors use `ok: false`, `result: null`, and:

```json
{
  "error": {
    "code": "application_author_mismatch",
    "message": "only the application author can publish releases",
    "details": {
      "current_signer": "...",
      "required_author": "..."
    }
  }
}
```

The initial stable error codes are:

- `not_logged_in`;
- `not_repository_maintainer`;
- `application_not_found`;
- `application_not_linked`;
- `application_author_mismatch`;
- `application_already_exists`;
- `application_already_linked`;
- `release_not_found`;
- `release_author_mismatch`;
- `release_already_exists`;
- `edit_target_not_found`;
- `asset_not_found`;
- `invalid_asset_author`;
- `invalid_asset_metadata`;
- `asset_platform_required`;
- `ambiguous_file_platforms`;
- `invalid_apk`;
- `apk_platform_conflict`;
- `asset_integrity_mismatch`;
- `duplicate_asset`;
- `release_platform_coverage_incomplete`;
- `partial_platform_confirmation_required`;
- `application_platform_update_required`;
- `ambiguous_selector`;
- `metadata_confirmation_required`;
- `relay_preflight_incomplete`;
- `publication_failed`;
- `replacement_ordering_exhausted`.

New error codes may be added in a backwards-compatible minor release. Existing
codes MUST NOT change meaning within `format_version: 1`.

### JSON entity shapes

Semantic entities have stable field names even when their source event is
invalid. Validation issues are objects containing `code`, `field`, and
`message`. `field` names the relevant NIP-82 tag or semantic field, and is
`null` when no individual field applies. The `details` object belongs only to
the separate runtime error and warning envelopes.

An application object has this shape:

```json
{
  "coordinate": "32267:<author>:ngit",
  "event_id": "...",
  "event_id_bech32": "nevent1...",
  "author": "<author>",
  "author_npub": "npub1...",
  "identifier": "ngit",
  "name": "ngit",
  "summary": "nostr git tooling",
  "description": "...",
  "icon": "https://example.org/icon.png",
  "images": [],
  "topics": ["git", "nostr"],
  "website": "https://gitworkshop.dev/ngit",
  "repository": "nostr://dan@gitworkshop.dev/ngit",
  "platforms": ["linux-x86_64"],
  "license": "MIT",
  "repository_coordinates": ["30617:...:ngit"],
  "created_at": 1786147200,
  "validation": [],
  "raw_event": {}
}
```

A release object has this shape:

```json
{
  "coordinate": "30063:<author>:ngit@1.8.0",
  "event_id": "...",
  "event_id_bech32": "nevent1...",
  "author": "<author>",
  "author_npub": "npub1...",
  "application_coordinate": "32267:<author>:ngit",
  "application_identifier": "ngit",
  "version": "1.8.0",
  "channel": "main",
  "released_at": 1786147200,
  "notes": "...",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "asset_ids": ["..."],
  "published_platforms": ["linux-x86_64"],
  "derived_platforms": ["linux-x86_64"],
  "validation": [],
  "raw_event": {}
}
```

An asset object has this shape:

```json
{
  "event_id": "...",
  "event_id_bech32": "nevent1...",
  "author": "<author>",
  "author_npub": "npub1...",
  "application_coordinate": "32267:<author>:ngit",
  "identifier": "org.ngit.cli",
  "version": "1.8.0+linux.1",
  "url": "https://cdn.example.org/ngit.tar.gz",
  "filename": "ngit.tar.gz",
  "mime": "application/gzip",
  "sha256": "...",
  "size": "18446744073709551615",
  "platforms": ["linux-x86_64"],
  "min_platform_version": null,
  "target_platform_version": null,
  "supported_nips": [],
  "variant": "glibc-2.17",
  "commit": "...",
  "min_allowed_version": null,
  "android": {
    "version_code": null,
    "min_allowed_version_code": null,
    "certificate_sha256": []
  },
  "original_url": null,
  "resolution": "resolved",
  "verification": null,
  "validation": [],
  "raw_event": {}
}
```

Release views return `application`, `release`, and `assets` using these shapes,
plus `unresolved_asset_ids`. List commands wrap arrays in `applications` or
`releases`; they never return a bare array.

Mutation results add:

```json
{
  "operation": "edited",
  "application_operation": "unchanged",
  "application": {},
  "previous_event_id": "...",
  "publication": {
    "ordered_events": [
      { "entity": "application", "event_id": "..." },
      { "entity": "asset", "event_id": "..." },
      { "entity": "release", "event_id": "..." }
    ],
    "relays": [
      {
        "url": "wss://relay.example.org",
        "status": "complete",
        "message": null
      }
    ],
    "possible_orphan_asset_ids": [],
    "recovery": null
  }
}
```

Release publication is reported as an ordered batch because the relay sender
can only prove that the complete sequence was acknowledged. A relay batch
status is `complete` or `incomplete`; ngit does not invent per-event results for
an incomplete batch. `ordered_events` is the exact send order, with the release
last. On total failure the same object is returned in `error.details`,
`possible_orphan_asset_ids` contains only assets newly signed by this command,
and `recovery` tells the caller which reuse flag is safe after checking those
exact event IDs. A newly created application remains valid if a later event in
the batch fails; a retry discovers and resends that exact event rather than
creating a replacement. URLs and relay messages which may contain credentials
are redacted in diagnostics; signed event URLs remain exact because changing
them would change the event.

## Application commands

### `ngit release app list`

The default lists trusted applications linked to the current repository.

Options:

- `--mine`: list applications authored by the active user, whether linked or
  unlinked;
- `--unlinked`: filter `--mine` to applications not linked to the repository;
- `--linked`: filter `--mine` to applications linked to the repository;
- `--author PUBKEY`: list applications by an explicit author; non-maintainer
  results are marked untrusted;
- `--relay URL`: extend relay discovery;
- `--offline`: use the cache only;
- `--json`: emit the JSON contract.

`--unlinked` and `--linked` require `--mine` unless `--author` is supplied.
The human table shows application identifier, name, author, linked state,
trusted state, and whether the active signer can publish.

For `--mine`, ngit MUST discover the user's applications through their NIP-65
relay preferences plus the repository and configured relay sets. This is not a
Zapstore account query. An offline result MUST be labelled as cache-limited.

The JSON result is:

```json
{
  "applications": [
    {
      "coordinate": "32267:<author>:ngit",
      "identifier": "ngit",
      "name": "ngit",
      "author": "<author>",
      "linked_repository_coordinates": ["30617:...:ngit"],
      "linked_to_current_repository": true,
      "trusted": true,
      "can_publish": true,
      "event_id": "...",
      "created_at": 1786147200,
      "raw_event": {}
    }
  ],
  "offline": false
}
```

### `ngit release app view APP`

This command displays every known application field, all repository links,
event identity, author, relay hints, link/trust state, and publication
authority. It can explicitly view an unlinked or untrusted address-qualified
application, but it MUST NOT present that application as belonging to the
repository.

The human view includes an actionable authority section. When another current
maintainer owns the application, it identifies that maintainer and states that
only they can edit the application or publish its releases.

### `ngit release app init`

This command creates an application owned by the active signer and links it to
the current repository. It accepts:

- `--id ID` (defaults to the repository identifier);
- `--name NAME`;
- `--description TEXT` or `--description-file PATH`;
- `--summary TEXT`;
- `--icon URL` and repeatable `--image URL`;
- repeatable `--topic TOPIC`;
- `--website URL`;
- `--repository URL`;
- repeatable `--platform PLATFORM`;
- `--license SPDX`;
- `--edit`;
- explicit `--clear-*` forms for optional fields;
- `--strict-metadata`;
- `--zapstore-relay` to additionally publish to the Zapstore catalog relay;
- `--json`.

Creation requires a name. Defaults MAY be inferred from repository metadata but
MUST be shown in a dry preflight before signing in interactive mode and in the
JSON mutation plan when `--json` is used. The command automatically adds every
current repository coordinate rather than requiring callers to construct `a`
tags.

An edit preserves all existing repository links, including links to other
repositories, unless a specific future unlink operation is requested. It also
preserves all unknown tags. `--edit` follows the strict state table above.

The mutation result contains `operation: "created"` or `"edited"`, the
application coordinate, event ID, previous event ID for edits, the normalized
semantic application, and per-relay publication results.

### `ngit release app link APP --edit`

This command adds every current repository coordinate to an existing
application. It is an application replacement, so `--edit` is required even
though the verb is already explicit.

`--zapstore-relay` additionally publishes the replacement to the Zapstore
catalog relay without changing any existing publication target.

The command MUST fail before signing when:

- the application does not exist;
- it is already linked to every current repository coordinate;
- the signer is not the application author;
- the signer is not in the current maintainer graph.

Existing metadata, unknown tags, and links to other repositories MUST survive
unchanged. Partial linkage is repaired by adding only missing current
coordinates and canonicalizing duplicates. The command MUST NOT copy or
re-publish another maintainer's application under the active signer's key.

## Release commands

### `ngit release list`

This command lists releases authored by the owners of trusted linked
applications. Releases are validated before display; an event is not trusted
solely because it matches an application identifier.

Options:

- `--app APP`;
- `--channel CHANNEL`;
- repeatable `--platform PLATFORM` with OR matching;
- `--author PUBKEY` for an explicit trusted application author;
- `--limit N`;
- `--relay URL`;
- `--offline`;
- `--json`.

The human table shows application, version, channel, release date, asset count,
platforms, author, and validation warnings. Results are ordered by release date
descending, then application identifier, version bytes, and event ID to make
ties deterministic.

The JSON list contains semantic release summaries. It does not need to resolve
complete asset events; missing cached assets are represented by an asset count
and warnings rather than silently dropping the release.

### `ngit release view RELEASE`

This command resolves the selected application, release, and every referenced
asset. The human output shows:

- release coordinate, event ID, author, date, version, channel, and notes;
- application coordinate, name, link state, and author;
- publication authority for the active signer, when known;
- every asset's event ID, URL, filename, MIME type, SHA-256, size, platforms,
  variant, commit, compatibility metadata, Android metadata, and original URL;
- unresolved asset IDs and all validation warnings.

`--verify` downloads every URL-backed asset and verifies its SHA-256 and size.
Viewing without `--verify` MUST NOT download asset bodies. A release remains
viewable when an asset is unavailable, malformed, or missing, but the invalid
reference MUST remain visible and the release MUST be marked incomplete.

The JSON result includes full `application`, `release`, and `assets` objects.
Each asset has a `resolution` value of `resolved`, `missing`, or `invalid`, plus
structured validation errors. A verifier result records the observed final
URL, byte size, and hash without changing the published event.

### `ngit release publish VERSION`

This command creates a release and its new URL- or file-backed asset events. It
accepts:

- `--app APP`;
- `--channel CHANNEL` (default `main`);
- `--notes TEXT` or `--notes-file PATH`;
- `--released-at UNIX_SECONDS` (default now for creation);
- `--tag TAG` when `{tag}` manifest expansion differs from VERSION;
- `--commit COMMIT` to override the Git revision represented by the release;
- `--manifest PATH`;
- repeatable `--asset PLATFORM=URL` for the simple case;
- `--file PATH` with repeatable `--platform PLATFORM` to upload one local file
  to Blossom for one or more platforms;
- repeatable `--file PLATFORM=PATH` as a compact multiple-file form; entries
  with the exact same `PATH` are one asset whose platforms are merged, so that
  file is snapshotted and uploaded only once;
- repeatable `--asset-event ASSET` to reuse an existing asset;
- `--platform-agnostic-asset URL` as an explicit no-platform shorthand;
- repeatable `--platform-agnostic-file PATH` as the corresponding local-file
  shorthand;
- `--accept-platform-agnostic-assets` to acknowledge reused asset events which
  have no `f` tags;
- `--add-application-platforms` to add release-only platforms to the
  replaceable application before publication;
- `--allow-partial-platforms` to acknowledge a non-main release which omits
  application platforms;
- repeatable `--blossom-server URL` as an ordered server override for all local
  files in the operation;
- `--zapstore-relay` to additionally publish the complete ordered batch to the
  Zapstore catalog relay;
- `--edit`;
- `--strict-metadata`;
- `--json`.

If there is one trusted linked application, omitting `--app` selects it. When
there are no trusted linked applications, the same command creates an
application owned by the active maintainer, using `--app`, the manifest
`application`, or the repository identifier as its identifier. Repository name,
description, topics, website, canonical clone URL, repository coordinates, and
the proposed release platforms seed its metadata. This zero-state path is the
default way to publish the application, asset, and release events together.
Multiple candidates remain an error. An exact existing but unlinked application
is never overwritten or linked implicitly; the user must run `release app link
APP --edit` first.

Creation requires at least one valid asset. A new release defaults its source
commit to the repository's current `HEAD`. `--commit` and top-level manifest
`commit` values may be any revision that resolves to a commit in the local
repository; ngit peels the revision and writes its full commit ID. On edit,
omitted notes, channel, release date, commit, and asset inputs preserve their
existing values. This includes preserving the absence of `commit` on a legacy
release. New asset inputs append; they do not replace or remove the existing
`e` tags. Asset removal and same-filename replacement require a future explicit
operation. An exact asset event already present in the release is an error, not
a silent no-op.

When local files are present, explicit `--blossom-server` values replace
discovery. The first server receives `PUT /upload`; every remaining server
receives `PUT /mirror` in argument order. Without an override, ngit uses the
ordered `server` tags from the latest kind `10063` event authored by the
application author, and fails rather than falling back when that latest event
is invalid. It fails before signing when neither source yields a server. Every
selected server is required in v1: a failed mirror aborts NIP-82 publication
rather than silently reducing the requested durability. Discovery requires at
least one completed author-relay route and reports other failed routes as
`relay_discovery_incomplete`; an explicit override is the deterministic
recovery when stale discovery is unacceptable.

Before publishing, ngit MUST:

1. resolve the latest application and release state across the relay set, or
   plan an initial application when neither exists;
2. apply the create/edit guard;
3. verify that the signer is both a current maintainer and application author;
4. resolve the selected Git commit and existing assets, download URL assets,
   and create stable snapshots of local files while hashing and validating
   their metadata;
5. resolve the ordered Blossom server set when local files are present;
6. construct the canonical union of asset platforms, apply the channel policy,
   and show or emit metadata warnings before the first signature;
7. upload each local snapshot to the first server and mirror it to every
   remaining server, validating every returned descriptor;
8. re-check application and release state;
9. sign the initial or additive application replacement when required, then
   new assets and the release;
10. publish one ordered application, assets, release batch, with the release as
    the final commit point.

The exact current application event is sent first even when it already exists.
This is a retry-safe duplicate and ensures a GRASP relay can validate each
new ngit asset's application `a` tag before accepting the dependent events.

Existing assets MUST be authored by the application author and satisfy the
required NIP-82 asset shape. When an asset has an application `a` tag, it MUST
match the selected application; absence remains valid for compatibility with
older publishers. Their `i` and `version` values MAY differ from the
application and release values and MUST be displayed without normalization. An
explicit asset event is never trusted only because its caller supplied the ID.
For newly created assets, ngit defaults those fields to the application
identifier and release version; manifest `identifier`/`version` or the asset
command's `--asset-id`/`--asset-version` override the defaults.

The mutation result includes the release coordinate, event ID, previous event
ID for edits, all asset IDs, newly published asset IDs, reused asset IDs,
per-relay results for every event, and a `blossom` member. Blossom output records
server-selection source, selected kind-10063 event ID when discovered, local
filename, hash, decimal-string byte size, MIME type, primary URL, and ordered
per-server operation, status, and descriptor URL. Status is `stored` for HTTP
201, `already_present` for HTTP 200, `failed` for a definite rejection,
`unknown` for an ambiguous transport outcome, or `not_attempted` after an
earlier fail-fast error. Mirror URLs are operational results; only the primary
URL is written to the kind `3063` event. A mutation without local files retains
the same shape with a null server selection and an empty upload list.
For APKs, each upload also contains `apk_platform_inference`, recording the
derived platforms, whether native libraries were present, and any ABI names
which ngit did not recognize.

## Asset commands

### `ngit release asset list RELEASE`

This is the compact counterpart to `release view`. It preserves release `e`
tag order and shows event ID, filename, MIME type, size, platforms, URL, and
resolution status. JSON contains complete semantic asset objects so scripts do
not need to scrape `release view` output.

### `ngit release asset view ASSET`

This command shows the complete kind `3063` event and parsed fields. With
`--release RELEASE`, it also validates author, required asset metadata, and
membership against that release. The asset identifier and version remain
independent descriptive values. `--verify` performs the same byte verification
as release view. Without release context, the CLI MUST label trust as unknown
rather than implying the event is an authoritative release asset.

### `ngit release asset add RELEASE --edit`

This command attaches exactly one asset to an existing release and replaces the
release. Because it overwrites an addressable release event, `--edit` is
mandatory.

The new asset is supplied by exactly one of:

- `--url URL`, with metadata flags such as repeatable `--platform`,
  `--platform-agnostic`, `--asset-id`, `--asset-version`, `--filename`,
  `--mime`, `--variant`, `--commit`, and Android fields; or
- `--file PATH`, with the same metadata flags and optional repeatable
  `--blossom-server URL`; or
- `--event ASSET`, for an existing immutable asset event.

`--zapstore-relay` additionally publishes the application, asset, and release
batch to the Zapstore catalog relay. It does not select Zapstore's Blossom CDN.

Metadata flags other than `--platform-agnostic` are invalid with `--event`
because an immutable event cannot be amended. In that form,
`--platform-agnostic` only acknowledges an existing event with no `f` tags; it
does not add metadata to the event.

`--add-application-platforms` and `--allow-partial-platforms` apply the same
platform policy as `release publish`. The former is the complete authorization
for an additive application replacement; `--edit` continues to authorize only
the release replacement.

The command first performs authority and release preflights, then downloads and
hashes a URL asset or snapshots and uploads a local file. It publishes one
ordered application, assets, release batch, resending the exact current
application and existing assets before the release replacement. If release
publication fails after the new asset succeeds, the asset is an unreferenced
but valid event; JSON and human output MUST report its ID and give safe recovery
steps. The user must inspect the exact release and asset event IDs, then reuse a
visible asset with `--event`; ngit must not suggest blindly repeating the source
command because that would sign a different immutable event.

The command preserves release notes, channel, original release date, unknown
tags, existing asset order, and existing asset IDs. It appends the new event ID
and recomputes the release platform union. A duplicate event ID or filename is
an error. ngit MUST never silently detach the older asset.

## Release manifest

The default project manifest location is `.ngit/release.yaml`. v1 reads it when
`--manifest` is supplied or, for release creation, when the default file exists
and no direct asset flags are given. Edits load a manifest only when
`--manifest` is explicit, so a metadata-only `--edit` cannot accidentally
re-append the creation assets. A missing default manifest is not an error when
direct assets are provided.

```yaml
schema: 1
application: ngit
channel: main
commit: main
assets:
  - source: https://downloads.example.org/ngit/{version}/ngit-linux-x86_64.tar.gz
    platforms:
      - linux-x86_64
    variant: glibc-2.17
    commit: 0123456789abcdef0123456789abcdef01234567
  - source: https://downloads.example.org/ngit/{version}/ngit-macos-arm64.tar.gz
    platforms:
      - darwin-arm64
  - source: https://cdn.example.org/ngit/{version}/checksums.txt
    platform_agnostic: true
  - file: dist/ngit-{version}-android-arm64-v8a.apk
    filename: ngit-{version}-android-arm64-v8a.apk
    mime: application/vnd.android.package-archive
    android:
      version_code: 10203
      certificate_sha256:
        - aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

Supported top-level fields are `schema`, `application`, `channel`, `notes`,
release-wide `commit`, and `assets`. An asset supports:

- exactly one of an HTTP(S) `source` URL or a local `file` path;
- `identifier` and `version`, defaulting to the application identifier and
  release version;
- `filename` and `mime` overrides;
- `platforms` or `platform_agnostic: true`;
- `min_platform_version` and `target_platform_version`;
- `supported_nips`;
- `variant`, `commit`, and `min_allowed_version`;
- `android.version_code`, `android.min_allowed_version_code`, and
  `android.certificate_sha256` as a list of one or more hashes;
- `original_url`.

Only the literal `{version}` and `{tag}` placeholders are expanded. `{version}`
is the exact VERSION argument. `{tag}` is the exact resolved Git tag and MUST be
provided explicitly when it differs from VERSION. URL substitutions are
percent-encoded as URL components; filenames and local paths use the literal
value. Relative local paths are resolved from the repository root and uploaded
through the same ordered Blossom workflow as `--file`. No shell, environment
variable, command, arbitrary template, or glob expansion is performed.

A local APK is the one exception to the general requirement for an explicit
`platforms` list: ngit derives its Android platforms from the exact stable
snapshot it hashes and uploads. This makes a manifest containing a local APK a
single-command zero-state workflow: when no NIP-82 events exist, `release
publish` creates the linked application, asset, and release and sends them in
that order. APKs cannot be marked `platform_agnostic`.

ngit requires one non-empty root `AndroidManifest.xml`. When the archive
contains native libraries under `lib/<abi>/*.so`, the supported ABI directories
become `android-<abi>` platforms; the standard Android ABI names use the NIP-82
spellings. Explicit platforms are merged only when they agree with those native
libraries. With no native libraries, ngit emits the four standard Android ABI
platforms and permits additional explicitly supplied `android-*` platforms.
Unknown but safely encoded ABI names are retained and reported with the
`apk_unknown_abi` warning. A declared non-Android platform, a platform absent
from an APK which has native libraries, a MIME conflict, or an invalid archive
fails before upload.

v1 deliberately does not parse the binary manifest or APK signing blocks.
`android.version_code` and at least one
`android.certificate_sha256` therefore remain explicit, validated manifest
metadata; `android.min_allowed_version_code` remains optional. This keeps ABI
inference small and auditable without pretending filename or archive layout can
prove package identity.

CLI values override top-level manifest values. For release commit selection,
precedence is `--commit`, top-level manifest `commit`, the prior value on edit,
then `HEAD` on create. Release-wide and per-asset commit values are independent:
the former identifies the source state represented by the release, while the
latter may identify the source of one particular artifact. Direct asset flags
append to manifest assets; they do not replace them. Duplicate final URLs,
local paths, or resolved filenames are rejected. Unknown schema versions and
unknown keys are errors so a typo cannot silently discard metadata.

## Metadata policy

Protocol-optional metadata remains optional on the wire, but ngit requires an
explicit decision where omission commonly creates a poor release:

- Each new asset requires at least one platform or an explicit
  `platform_agnostic: true`/`--platform-agnostic` acknowledgement, except that
  local APK platforms may be inferred from the archive snapshot.
- A missing filename or MIME type is inferred from the final URL, response
  headers, and content sniffing. Ambiguous or generic results produce a warning.
- Android APK assets require an explicit version code and signing certificate
  SHA-256. Local APK platform tags are inferred from the stable archive
  snapshot and merged with compatible declarations.
- Applications SHOULD have a summary, icon, website, repository URL, license,
  and platform hints. Creation warns about omissions but does not require them.
- Releases SHOULD have non-empty notes. ngit-created releases identify a source
  commit by default; legacy edits preserve an omitted tag unless explicitly
  overridden.
- `--strict-metadata` promotes all metadata warnings to validation errors.

This policy makes users acknowledge absent platform metadata without inventing
an NIP-82 tag for platform agnosticism. Imported immutable assets which have no
`f` tags require the same explicit `--platform-agnostic` acknowledgement before
they can be added.

Platform values are trimmed, non-empty, case-sensitive strings in v1. ngit
deduplicates exact values and SHOULD suggest the identifiers in NIP-82 Appendix
A, while still allowing custom values because ngit does not own the platform
taxonomy. The human preflight SHOULD flag suspicious spelling or case without
silently rewriting it.

### Application and release platform relationship

Application `f` tags are ngit's current-channel interoperability baseline, not
an assertion that every future channel must forever ship every platform.
Publication follows these rules:

- A `main` release MUST contain an asset for every application platform.
  `--allow-partial-platforms` cannot weaken this invariant because current
  Zapstore clients may choose the newest release without channel or platform
  filtering.
- A `main` release containing an additional platform fails by default. Passing
  `--add-application-platforms` additively replaces the application, preserving
  every other typed field, repository link, and unknown tag, and publishes that
  replacement before the assets and release.
- A non-main release may introduce an experimental platform without changing
  the application. Passing `--add-application-platforms` explicitly promotes
  those additional platforms into the application baseline.
- A non-main release may omit application platforms only with
  `--allow-partial-platforms`. JSON sets `partial_release: true` and emits a
  `partial_platform_release` warning about current client compatibility.
- A platform-agnostic asset contributes no platform and never satisfies an
  application platform requirement.

There is intentionally no generic `--force`: platform coverage and application
replacement are separate decisions with separate flags. Mutation JSON includes
`application_operation`, `previous_application_event_id`, and a
`platform_policy` object containing the application, release, missing,
additional, added, and resulting platform sets.

## Event construction and replacement ordering

Publication order is:

1. the newly created, explicitly updated, or exact current application event;
2. immutable asset events;
3. the addressable release event.

The release is the final commit point because it is the authoritative list of
assets. Publishing a release before its new assets creates avoidable broken
references.

Application and release edits MUST preserve unknown tags while regenerating all
managed tags. They MUST use NIP-01's addressable-event ordering rule. For a
release edit, ngit first preserves the original `created_at` release date and
uses bounded nonce grinding to produce a lower event ID at the same timestamp.
Unlike ordinary repository state, it MUST NOT silently advance the timestamp as
a fallback because clients interpret it as the release date. Exhaustion fails
with `replacement_ordering_exhausted`; changing the date requires an explicit
`--released-at` value.

Applications do not assign release-date semantics to `created_at`, so their
replacement may use ngit's normal same-timestamp ordering and next-timestamp
fallback policy described in
[event created-at ordering](architecture/event-created-at-ordering.md).

Required events are sent to each selected publication relay in asset order with
the release last; sending stops on that relay at the first unacknowledged event.
Existing signed assets MAY be re-published verbatim. A mutation succeeds only
when at least one relay acknowledges the complete ordered batch. Partial batch
completion is present in JSON, but it is not expanded into per-event certainty:
an event whose acknowledgement failed may still have reached the relay. If no
complete relay is available, the operation fails and reports only the newly
signed application ID and asset IDs which may now be orphaned.

## Discovery and validation

Implementations MUST apply these filters and then validate the returned events:

1. Discover the repository's current maintainer graph and coordinates.
2. Fetch kind `32267` applications whose authors are current maintainers and
   whose `a` tags intersect current repository coordinates.
3. For `--mine`, independently fetch kind `32267` events authored by the active
   user, without requiring a repository `a` tag.
4. Fetch kind `30063` releases by the exact application author and application
   identifier. Validate `a`, `i`, `d`, version, channel, and author consistency.
5. Fetch kind `3063` assets by the exact IDs in the trusted release. Validate
   each asset's author and required metadata before treating it as
   authoritative. Preserve independent asset identifiers and versions.

The latest addressable event is chosen by greatest `created_at`, then lowest
event ID for equal timestamps. Invalid newer events MUST be surfaced as invalid;
ngit MUST NOT silently fall back to an older valid revision and present it as
latest.

## Blossom publication

Local-file publication uses a small private Blossom transport rather than
vendoring or git-depending on the pending rust-nostr branch. Doing so avoids a
second incompatible Nostr type graph and keeps the stacked change limited to
the BUD operations the release workflow needs. The transport can be replaced
by a released upstream client later without changing the CLI or JSON contract.

ngit copies each input file into a private temporary snapshot while computing
its SHA-256 and checked `u64` size. It rejects files larger than the same 4 GiB
limit used for URL acquisition. Hashing, upload, and retry all read that stable
snapshot so a build process cannot change the described bytes between passes;
the complete file is never buffered in memory. The original path and any
credential-bearing URL are not printed in authorization events.

For each request, ngit signs a short-lived kind `24242` authorization containing
`t=upload`, `x=<lowercase sha256>`, and an expiration tag. The HTTP
`Authorization` value is `Nostr ` followed by URL-safe, unpadded base64 of the
signed event JSON. Authenticated PUT requests never follow redirects.

The primary request is `PUT /upload` with `Content-Length`, `Content-Type`, and
`X-SHA-256` headers and the snapshot as its streaming body. A mirror request is
`PUT /mirror` with JSON `{ "url": PRIMARY_URL }`, `X-SHA-256`,
`X-Content-Length`, and `X-Content-Type`. Both endpoints may return `200` or
`201`. ngit requires a valid descriptor whose hash, size, MIME type, and
HTTP(S) URL agree with the local snapshot. A mismatched or malformed response
is a failure even when its status code is successful.

Upload and mirror operations are sequential and ordered. The primary upload
must succeed before any mirror is attempted, and every selected mirror must
succeed before ngit signs a kind `3063` asset or kind `30063` release. A later
failure reports every observed server result plus the hash and primary URL as a
possible orphan blob; it does not claim that a Nostr asset exists and does not
recommend a blind rerun. Payment-required and authentication-challenge
responses are reported as unsupported, actionable failures in v1.

A Blossom failure uses code `blossom_publication_failed`. Its details include
the failed stage and server, the complete ordered server plan, and
`release_events_signed: false` plus `release_events_published: false`.
`possible_orphan_blobs` contains HTTP-201 locations created by this invocation
and ambiguous requests whose storage result is unknown; it excludes HTTP-200
blobs which were already present. HTTP 5xx responses are ambiguous because a
server may fail after storing the bytes, so they use status `unknown` and add a
hash-only possible orphan; 3xx/4xx responses remain definite rejections.
Recovery explains that blob publication is
content-addressed and may be retried after fixing the server set. No automatic
orphan deletion is attempted. If a later state check, signing operation, or
relay publication fails, its existing error code and details are retained and
enriched with the completed Blossom report and possible orphan blobs. Human
errors include the ordered server outcomes, possible orphan locations, signed
application ID, asset IDs, release-signature state, and recovery guidance. JSON
represents the same downstream progress explicitly as
`signed_application_id`, `signed_asset_ids`,
`release_event_signed`, and `publication_complete`; it never infers that all
release events were signed merely because one asset event was signed.

## Gotchas and edge cases

This section is a safety brainstorm and implementation checklist. Items here
are not permission to weaken the normative rules above; unresolved cases should
fail closed with an actionable error.

### Authority and repository identity

- A current maintainer can publish repository events but cannot publish for an
  application owned by another maintainer. Check this before downloading a
  multi-gigabyte asset.
- A malicious non-maintainer can add the repository's `a` tag to their app.
  Repository linkage is trusted only when the author is in the current
  directional maintainer graph.
- A maintainer can be invited but not confirmed. The release trust decision
  must deliberately use the same directional authorization model as repository
  events while presenting the relationship honestly.
- An application author can later leave or be removed from the maintainer
  graph. Existing releases remain addressable history, but the application is
  no longer a trusted linked application for new default discovery or
  publication.
- The maintainer graph can change between preflight and signing. Re-resolve it
  immediately before signing, and abort if authority changed.
- Different selected maintainer coordinates can discover different graph
  views. Include the selected coordinate and complete evaluated graph in JSON
  so the decision can be reproduced.
- Same repository identifiers under unrelated maintainer graphs are not the
  same repository. Never trust an identifier-only match.
- Linking must include all current repository coordinates in canonical order,
  including invited maintainers, without deleting stale or foreign repository
  links.
- A signer may own an unlinked app but not currently maintain the repository.
  Show it under `--mine --unlinked`, but do not allow linking.
- Another maintainer's application must never be copied under the current
  signer's key as an attempted workaround for author mismatch.

### Relay discovery and stale data

- NIP-65 preferences can be absent, malformed, stale, or point only to offline
  relays. Fall back to configured discovery routes, but report which routes
  were incomplete.
- Relays can disagree about the latest replaceable event. Merge results using
  NIP-01 ordering after signature and address validation, not response order.
- A relay can return an older application or release after accepting a newer
  one. The mutation result should be based on the signed event and acceptance
  acknowledgements, not an immediate unqualified read-back.
- A relay may close before end-of-stored-events or enforce filters incorrectly.
  Such a route cannot prove a safe create preflight.
- Offline cache entries can be partial or stale. Offline commands must label
  this limitation; writes are not allowed offline in v1.
- No finite relay set proves global non-existence. Document the checked set in
  verbose and JSON output and query every publication relay before create.
- Relay hints embedded in `naddr`/`nevent` are discovery hints, not trust roots.
- Duplicate relay URLs, trailing slashes, case, default ports, redirects, and
  `ws`/`wss` equivalence need canonical connection handling without changing
  the URL strings preserved in signed events.
- A user may have applications only on a relay unknown to the repository. The
  `--relay` escape hatch and NIP-65 discovery are essential for linking them.
- Large unbounded author queries can be expensive. Paginate or bound relay
  requests without silently returning a complete-looking partial list.

### Selector and address ambiguity

- The same application identifier can be published by several maintainers.
  Bare IDs must fail when ambiguous, even if names and icons match.
- The same version can exist for several linked applications. Bare versions
  must require `--app` unless there is exactly one candidate.
- `v1.0.0`, `1.0.0`, case differences, Unicode normalization forms, and leading
  or trailing whitespace can create distinct addresses. Reject surrounding
  whitespace and control characters but do not otherwise normalize identity.
- Application identifiers containing `@` make `<app>@<version>` parsing
  ambiguous. Either reject `@` when creating ngit-managed app IDs or require an
  address-qualified selector; do not split at an arbitrary occurrence.
- A release `naddr` identifies an address, while an event ID/`nevent` may point
  to an obsolete revision. Views should show both requested and latest revision
  status. Edits always target the latest known address state.
- Short hex prefixes and filenames can collide. Do not accept them unless the
  command has a bounded context and proves uniqueness.
- Malformed bech32, wrong-kind coordinates, wrong network data, and valid event
  IDs of the wrong kind need distinct validation errors.

### Addressable overwrite safety

- A create can overwrite an event visible only on an unavailable relay. Refuse
  when a selected publication relay cannot complete preflight.
- `--edit` must never create a missing address. This catches misspelled app IDs
  and versions which would otherwise fork the release catalogue.
- Concurrent editors can both preflight the same revision. Before signing, do
  a final read and compare the previous event ID; abort on change instead of
  losing the other edit.
- Same-second edits require the lower-ID NIP-01 tie-break. Bounded grinding must
  have a deterministic deadline and report exhaustion.
- Release `created_at` is also its release date. A generic next-second fallback
  would corrupt that meaning, so release edits fail unless the user explicitly
  changes the date.
- An existing event with a timestamp far in the future can make replacement
  difficult and distort ordering. Refuse implausible timestamps by default and
  explain the recovery path rather than chasing them silently.
- Re-signing an identical event can reproduce the same ID. Treat that as no
  change, not a successful edit.
- Patch edits must preserve unknown tags but remove duplicate managed tags.
  Otherwise future metadata is lost or attacker-controlled duplicate values
  survive.
- Explicit clearing flags must distinguish absent, empty, and preserved values.
  Empty strings should not accidentally generate malformed singleton tags.

### Application linking

- An app may link one current repository coordinate but omit newer maintainer
  coordinates. `app link --edit` should repair partial linkage rather than call
  it fully linked.
- An app may link several unrelated repositories. Linking this one must not
  remove any of them.
- An application with the desired identifier may already exist for the active
  author on a relay not used by the initial list operation. The create preflight
  must use the broader publication/discovery set.
- Duplicate `a` tags, invalid coordinates, relay-hinted tag extensions, and
  future tag fields must be canonicalized or preserved deliberately.
- An existing app may omit required NIP-82 fields or contain conflicting
  singleton tags. Viewing should expose invalidity; editing must not silently
  legitimize ambiguity without an explicit correction.
- A repository clone URL can change without the application being edited.
  Avoid presenting it as guaranteed current, and do not replace a user-chosen
  repository URL merely because inference found another mirror.
- Publishing a release can reveal platforms absent from the application's `f`
  tags. Fail by default. Only `--add-application-platforms` authorizes an
  additive application replacement; preserve all unrelated metadata and links.

### Release consistency

- Validate that release author, application `a`, `i`, `d`, and version agree.
  Matching only `#i` can attach a release to the wrong application author.
- A release may contain no assets, duplicate `e` tags, a nonexistent asset, or
  an event ID of the wrong kind. Keep it viewable but visibly invalid.
- Release `f` tags can omit asset platforms, contain extras, or contain
  duplicates. Display the published and derived unions and warn on mismatch.
- A replacement can accidentally lose notes, channel, date, assets, or future
  tags. Patch from the exact preflight revision and test every preserved field.
- Adding an asset to a release while another process edits it can lose one
  process's asset. Compare-and-retry or abort on previous-ID mismatch.
- Assets with identical bytes but different event IDs are not exact duplicates.
  Warn about the duplicate hash and require an explicit decision.
- Asset identifiers and versions are deliberately independent from application
  and release identifiers and versions. Do not reject a package such as
  `com.example.android` from a release for a cross-platform `com.example.app`,
  and do not rewrite either value for display.
- An asset's application `a` pointer is optional for compatibility. When it is
  present, validate its kind, author, and exact coordinate against the release;
  do not infer a missing pointer while reading an immutable legacy event.
- Custom channel names, empty channels, control characters, and case variants
  need validation. Do not silently map `Main` to `main`.
- Notes can be empty, extremely large, non-UTF-8 on disk, or contain terminal
  control sequences. Bound event size, require UTF-8, and sanitize display.

### Asset acquisition and integrity

- Redirect loops, excessive redirects, HTTPS-to-HTTP downgrade, unsupported
  schemes, DNS changes, and a final URL different from the source URL need a
  documented policy. Record the stable intended public URL, not an expiring
  redirect target, unless the user explicitly chooses otherwise.
- URLs can contain `=`, `;`, fragments, Unicode, percent escapes, and shell
  metacharacters. Split the simple `PLATFORM=URL` form only at the first `=` and
  never execute it through a shell.
- Query strings often contain bearer tokens or expiring signatures. Since the
  complete URL becomes public Nostr data, warn prominently and reject likely
  secrets in strict mode.
- Localhost, private-network, and file URLs may work for the publisher but not
  consumers. v1 accepts only HTTP(S) and warns about non-public hosts.
- `Content-Length` can be absent or false, and content encoding can change the
  bytes. Hash and count the actual decoded-or-raw representation selected by a
  fixed HTTP policy, disable transparent transformations which would make a
  later download hash differently, and test it.
- Downloads need connect, idle, and total deadlines, redirect bounds, and a
  configurable byte limit. Stream hashes; do not buffer whole assets in memory.
- Servers can change bytes between preflight and consumer download. ngit can
  prove only what it observed; `release view --verify` detects later drift.
- MIME hints from extension, `Content-Type`, and magic bytes can conflict.
  Prefer observed content, preserve the disagreement as a warning, and never
  publish `Content-Type` parameters as the NIP MIME value.
- `Content-Disposition` filenames can contain path traversal, absolute paths,
  NULs, bidirectional controls, or terminal escapes. Store only a sanitized
  basename and preserve the raw header only in diagnostics.
- Hash strings can vary in case or include prefixes. Emit lowercase 64-character
  SHA-256 hex and reject other algorithms where NIP-82 expects `x`.
- A Blossom URL conventionally embeds a hash. Compare that hash with observed
  bytes and fail on mismatch instead of publishing internally contradictory
  metadata.
- Byte sizes can exceed signed or JavaScript integer limits. Use checked `u64`
  internally and decimal strings in JSON.
- Two assets can resolve to the same filename. Refuse implicit replacement even
  when URLs or hashes differ.
- A failed release publication can leave a valid orphan asset. Report it and
  make retry reuse the same event rather than creating endless duplicates.
- HTTP authentication, cookies, custom headers, and private URLs risk leaking
  secrets into logs or events. They are out of v1 unless a separate secret-safe
  acquisition design is added.

### Platform and package metadata

- No `f` tag can mean platform-independent or merely missing metadata. Requiring
  explicit acknowledgement keeps that ambiguity visible to the publisher.
- Platform strings have no ngit-owned universal taxonomy. Suggestions must not
  silently rewrite values used by another ecosystem.
- Universal binaries, source archives, checksums, containers, browser
  extensions, and firmware do not map neatly to OS/architecture pairs. Allow an
  explicit platform-agnostic decision and multiple opaque platforms.
- Duplicate platform tags with different case should be warned about, not
  folded automatically.
- Release `f` is derived from assets. A platform-agnostic asset contributes no
  tag and must not erase platforms contributed by other assets.
- Current Zapstore release and asset lookups do not consistently include
  channel and platform filters. A partial beta/nightly release can therefore be
  selected as if it were the main release; keep the explicit acknowledgement
  until those clients are known to be channel-aware.
- An additive application platform replacement can be accepted while a later
  asset or release is rejected. Retry from observed relay state and resend the
  accepted application event; never sign a second replacement merely to make
  the batch look atomic.
- Updating application platforms must preserve links to other repositories and
  unknown future tags. Reconstructing only the fields ngit understands can
  silently sever another publisher's metadata.
- APK architecture inference must inspect the same immutable bytes being
  hashed. v1 derives ABI platforms only; version code and certificate hashes
  stay explicit until a small, independently audited parser can validate them.
- APKs can be split, universal, signed by multiple certificates, unsigned, or
  use signing schemes the parser does not understand. Fail safely rather than
  choosing an arbitrary certificate.
- A Java/Kotlin-only APK has no native ABI directories. Treat it as supporting
  the four standard Android ABIs, merge only explicit `android-*` additions,
  and never call it platform agnostic.
- Native ABI directory names can be novel. Preserve safe names as
  `android-<abi>` with a structured warning; reject hostile archive names and
  bound ZIP entry traversal work.
- Certificate hashes and Android integers need canonical encoding and overflow
  checks. Version code zero may be technically encodable but should be treated
  deliberately.
- Archives may contain several binaries for different platforms. Filename
  inference alone is insufficient; require user confirmation.

### Manifest parsing and precedence

- YAML duplicate keys can make two parsers see different values. Reject
  duplicates, aliases, and merge keys unless semantics are deliberately
  specified.
- Unknown keys and schema versions must fail, not warn, because a typo can drop
  critical platform or integrity metadata.
- Templates must never expand shell commands, environment variables, arbitrary
  Git fields, or path globs. URL-encode substitutions where required and show
  the resolved URL before publication.
- VERSION and Git tag can differ. `{tag}` must fail if no exact tag was supplied
  or resolved unambiguously.
- A release commit input can be a branch, tag, abbreviated ID, or revision
  expression. Resolve it locally, require it to peel to a commit, and serialize
  the full ID so consumers never inherit the publisher's revision ambiguity.
- An edit with no commit input must preserve the existing `commit` tag exactly,
  including preserving absence for a legacy release. An asset-add replacement
  must do the same.
- CLI-over-manifest precedence must be field-specific and documented. Repeated
  assets append; scalar overrides must not duplicate singleton tags.
- Relative manifest and local asset paths are resolved from the repository
  root, not the caller's current subdirectory.
- Symlinks, changing files, and files modified during upload must not let the
  described bytes drift. Local sources use the same stable private snapshot as
  direct file arguments; path globs are never expanded.
- Resolved manifests and JSON plans can expose credential-bearing URLs. Redact
  diagnostics, while warning that the URL itself would still be public in the
  signed event.

### JSON and terminal behavior

- Update notices, signer prompts, logging, and spinners can corrupt JSON stdout.
  Test that every `--json` path emits exactly one parseable object.
- Failures after some events publish still need one terminal JSON object listing
  the ordered event IDs, batch-level relay results, possible orphan asset IDs,
  and safe recovery guidance. Never stream partial JSON entities or claim an
  exact per-event result when an acknowledgement is uncertain.
- Deterministic ordering matters for tests and consumers: sort discovered
  entities, but preserve semantically ordered event tags such as release assets.
- Distinguish `null`, empty arrays, empty strings, missing remote data, and
  protocol-invalid data in the schema.
- Raw events can contradict parsed semantic fields. Keep both and attach
  validation errors; do not rewrite the raw representation.
- Pubkeys and event IDs should have one canonical machine representation
  (lowercase hex), with `npub`/`nevent` display forms in separate fields.
- Remote names, filenames, notes, and relay messages can contain ANSI escapes,
  bidi controls, or newlines. Sanitize human output without altering JSON data.
- Broken pipes and Ctrl-C during publication can leave uncertain relay state.
  On retry, preflight exact event IDs and report whether the intended mutation
  already landed.
- Human-readable warnings on stderr should have corresponding structured
  warning codes in JSON, not exist only as prose.

### Publication and retry behavior

- Relays can accept assets and reject the release, accept an event before timing
  out, or return conflicting acknowledgements. Report whether each relay
  completed the ordered batch, without inferring which prefix is visible.
- The release must be published last, but Nostr offers no multi-event atomic
  transaction. Consumers must tolerate temporarily missing assets and producers
  must expose partial failure precisely.
- A retry after uncertain acknowledgement should first query the exact event ID
  and addressable coordinate. Reuse any visible asset with `--asset-event` for
  `release publish` or `--event` for `release asset add`; only rerun once the
  observed state determines safe arguments, and never create a different asset
  event for the same bytes and metadata by default.
- Different relays may impose different maximum event sizes or reject old
  release timestamps. Preflight cannot guarantee acceptance; surface relay
  policy failures without changing event semantics per relay.
- Signing through a remote signer can fail, return the wrong pubkey, or be
  cancelled between assets and release. Verify each returned event locally and
  re-check that all events share the required author.
- Clock skew affects new release dates and replacement ordering. Use a stable
  captured timestamp for the plan and show it before signing.

### Blossom uploads and mirroring

- Blossom servers can return `200` or `201`, a body which does not match the
  uploaded hash, authentication challenges, payment requirements, or a URL on a
  different host. Validate the upload descriptor against local bytes.
- BUD-03 discovery ordering matters. Preserve user/server order and do not turn
  a fallback list into nondeterministic parallel preference.
- BUD-04 mirroring can partially succeed. v1 requires every selected server,
  reports each result, and signs no NIP-82 event after a partial upload.
- BUD-10 Blossom URIs are not accepted as returned primary URLs in v1. Require
  an ordinary HTTP(S) URL that existing NIP-82 clients can retrieve.
- Upload authorization events have narrow lifetimes and scopes. Never cache or
  print secrets, create one close to each request, and account for remote-signer
  latency and clock skew.
- Servers may deduplicate by hash while serving different headers or filenames.
  NIP-82 integrity is byte-based; display metadata still needs deterministic
  selection.
- NIP-82 currently exposes a primary URL. Keep mirror URLs in command results
  and do not invent extension tags without protocol agreement.
- A local file can be replaced, truncated, grow, or be a symlink into mutable
  build output while ngit is running. Upload only a completed private snapshot
  and fail on read errors or the configured size bound.
- Snapshotting can exhaust temporary storage even when the source file is
  within the byte limit. Surface that failure without signing or uploading.
- Server-list events can contain malformed roots, credentials, paths,
  duplicates, or mixed schemes. Accept only canonical credential-free HTTP(S)
  roots, preserve the first occurrence, and never reorder fallbacks.
- Explicit servers are an override, not an addition to discovery. Mixing the
  two would make the durability set hard to predict and review.
- Reject returned descriptor URLs with credentials, queries, fragments, or no
  embedded asset hash; an expiring download URL is not a stable NIP-82 source.
- `.onion` HTTP servers are unusable unless the HTTP client has an explicit
  proxy path; relay onion handling does not make reqwest reach them.
- With multiple files, later failure can leave earlier blobs stored. Preserve
  every completed file report and never attempt automatic deletion.
- A state, signing, or relay failure after successful Blossom work must retain
  the completed upload report alongside any Nostr orphan-asset report.

### Test coverage checklist

- Unit-test every create/edit state-table cell for both applications and
  releases.
- Unit-test authority combinations: author/non-author, maintainer/non-maintainer,
  linked/unlinked, invited/confirmed, and author removed after discovery.
- Test addressable same-timestamp replacement using bounded observable
  conditions, never fixed sleeps.
- Test concurrent previous-event changes and ensure the later publisher aborts
  rather than losing data.
- Test application edits preserve unknown tags and links to other repositories
  while canonicalizing managed duplicates.
- Test release edits preserve notes, channel, date, unknown tags, and existing
  asset order.
- Test asset publication precedes release publication and partial failures
  expose reusable orphan asset IDs.
- Test missing, wrong-kind, wrong-author, malformed, duplicate, and
  hash-identical asset events, plus valid independent asset IDs and versions.
- Test HTTP redirects, timeouts, false lengths, oversized streams, changing
  bytes, MIME conflicts, hostile filenames, and Blossom hash mismatch with a
  bounded local server.
- Test stable local snapshots, kind-24242 scope/expiration, `200` and `201`
  descriptors, redirect refusal, ordered kind-10063 discovery, explicit
  override precedence, primary upload followed by mirrors, and a mirror failure
  which publishes no NIP-82 events.
- Test manifest duplicate keys, unknown keys, precedence, exact placeholder
  expansion, and credential redaction.
- Parse JSON structurally in integration tests; do not assert exact human
  stdout. Assert stdout is a single JSON object and 64-bit values are strings.
- Keep integration tests parallel and isolated: no `#[serial]`, process-global
  environment mutation, PTY automation, or wall-clock sleeps.

## Acceptance criteria for v1

The first implementation is complete when:

1. all commands in the command summary exist and support `--json`;
2. default discovery lists only trusted applications linked to the repository;
3. `app list --mine --unlinked` can find an owned application through normal
   Nostr relay discovery and `app link --edit` can link it safely;
4. human and JSON views expose application ownership and publication blockers;
5. applications and releases cannot be replaced without `--edit`, and
   `--edit` cannot create them;
6. an asset can be added to an existing release without losing prior release
   fields or assets;
7. URL-backed assets are streamed, hashed, described, published before the
   release, and optionally verified on read;
8. local files are snapshotted, uploaded and mirrored in order, and no NIP-82
   event is signed until every required Blossom operation succeeds;
9. platform metadata is supplied or its omission is explicitly acknowledged;
10. edits preserve unknown tags and use safe addressable-event ordering;
11. JSON remains parseable and useful on success, validation failure,
    authorization failure, and partial relay publication.
