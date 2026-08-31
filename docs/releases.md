# Publishing releases

`ngit release` publishes NIP-82 Software Application, Software Asset, and
Software Release events linked to the current repository. The default project
manifest is `.ngit/release.yaml` (singular).

## Quick start

For one local file, pass its path and every platform supported by those exact
bytes:

```sh
ngit release publish "$VERSION" \
  --file dist/my-app \
  --platform linux-x86_64 \
  --platform linux-aarch64 \
  --json
```

The file is snapshotted once, confirmed on every selected Blossom server, and
represented by one asset event with two `f` platform tags. Ngit skips exact
copies and directly uploads missing ones with bounded retries and post-upload
verification before it signs the event. On a repository with no existing
NIP-82 events, the same command creates and publishes the linked application,
asset, and release in dependency order.

For an existing application, a `main` release must cover every application
platform. If the release intentionally introduces a new main-channel platform,
add `--add-application-platforms`; ngit will otherwise fail rather than silently
replace the application. Non-main subsets require `--allow-partial-platforms`.

The compact shorthand remains useful for several files:

```sh
ngit release publish "$VERSION" \
  --file linux-x86_64=dist/my-app-x86_64 \
  --file linux-aarch64=dist/my-app-aarch64 \
  --json
```

Repeating the exact same path merges its platforms into one asset:

```sh
ngit release publish "$VERSION" \
  --file linux-x86_64=dist/my-app-universal \
  --file linux-aarch64=dist/my-app-universal \
  --json
```

Use `--platform-agnostic-file PATH` only for files such as checksums or release
notes which genuinely apply to every platform. A local APK has its Android
platforms inferred from its native ABI directories, but still requires the
Android metadata described below.

## Publishing to the Zapstore catalog relay

Release events normally go to the repository relays and the application
author's NIP-65 write relays. Add `--zapstore-relay` to a release mutation to
also target `wss://relay.zapstore.dev`:

```sh
ngit release publish "$VERSION" \
  --file dist/my-app \
  --platform linux-x86_64 \
  --zapstore-relay \
  --json
```

The flag adds the same publication target as
`--relay wss://relay.zapstore.dev`, but it does not add Zapstore to general
event or Blossom server-list discovery. It does not replace any existing
publication target, select Zapstore's CDN, change the application's Blossom
server list, or perform Zapstore whitelisting and certificate-linking setup.
Relay acceptance is included in the normal human and JSON publication results.

## The release manifest

A manifest is useful when filenames and metadata are stable across releases.
With `.ngit/release.yaml` committed, release creation needs no asset arguments:

```sh
ngit release publish "$VERSION" --json
```

One manifest asset represents one set of bytes. Put every platform for those
bytes in that asset's `platforms` list; do not repeat the file entry:

```yaml
schema: 1
identifier: com.example.my-app
pubkey: npub1expectedpublisher...
name: Example App
summary: A short store listing summary
description: |
  A longer application description.
tags: [nostr, productivity]
license: MIT
website: https://example.com
repository: nostr://npub1maintainer.../example
icon: assets/icon.png
images:
  - assets/screenshot.png
  - https://cdn.example.com/existing-screenshot.png
channel: main
release_notes: CHANGELOG.md
publication:
  blossom_servers:
    - https://blossom.example.com
    - https://mirror.example.com
  relays:
    - wss://releases.example.com
  zapstore_relay: true
assets:
  - file: artifacts/my-app-{version}
    filename: my-app-{version}
    mime: application/octet-stream
    platforms:
      - linux-x86_64
      - linux-aarch64
    commit: 0123456789abcdef0123456789abcdef01234567
  - file: artifacts/checksums.txt
    platform_agnostic: true
```

Relative paths are resolved from the repository root, not the process's current
directory. `{version}` expands to the exact positional VERSION. `{tag}` expands
to `--tag TAG`; supply `--tag` when it differs from VERSION. There is no shell,
environment-variable, glob, or arbitrary template expansion.

Top-level fields are:

- `schema`: required and currently `1`;
- `application`: application identifier, optional when repository discovery is
  unambiguous;
- `identifier`: Zapstore-compatible alias for `application`; the two are
  mutually exclusive;
- application metadata: `pubkey`, `name`, `summary`, `description`, `tags`,
  `license`, `website`, `repository`, `icon`, `images`, and `communities`;
- `supported_nips`: default for assets which do not set their own list;
- `channel`: defaults to `main` on creation;
- `notes`: literal inline release notes;
- `release_notes`: repository-relative or absolute Keep a Changelog file;
- `publication`: stable transport and release-policy defaults for CI;
- `assets`: one or more asset objects.

Application metadata uses the same top-level names as `zapstore.yaml` where
their meanings agree. `pubkey` is an expected-publisher guard, not a secret:
publication fails before upload when it does not match the active signer.
Supplied metadata creates the application on the first release and replaces a
linked application when its declared values change; omitted fields retain an
existing value or use repository metadata during creation.

For `icon` and `images`, an HTTP(S) URL is retained exactly as supplied and is
never downloaded or re-uploaded. Any other value is a repository-relative
local file: it must be tracked by Git, resolve inside the repository, have an
image MIME type, and be no larger than 20 MiB. Local images are snapshotted and
confirmed on every selected Blossom server before any NIP-82 event is signed;
the application event uses the first server's returned URL. Thus local media is
Blossom-first while existing CDN or Blossom URLs remain usable as references.

`notes` and `release_notes` are mutually exclusive. For `release_notes`, ngit
selects the level-two section matching the exact positional VERSION, allowing
the conventional optional leading `v` in either the VERSION or heading. It
accepts standard headings such as `## [1.2.3] - 2026-08-31` as well as
unbracketed `## 1.2.3`, preserves the section's Markdown, and stops at the next
level-two heading. A missing, duplicate, or empty matching section fails before
publication instead of publishing the entire changelog. Relative paths resolve
from the repository root.

CLI `--notes` and `--notes-file` take precedence over either manifest field.
`--notes-file` remains literal and does not perform changelog extraction.

The optional `publication` block supports:

- `blossom_servers`: ordered required placements; the first supplies the asset
  event URL and every missing copy is uploaded directly;
- `relays`: additional discovery and publication relays;
- `zapstore_relay`: add `wss://relay.zapstore.dev` as a publication-only
  target;
- `strict_metadata`: fail instead of publishing with metadata warnings;
- `allow_partial_platforms`: allow a non-main release to cover only part of
  the application's platform set;
- `add_application_platforms`: extend the application when a main release
  introduces platforms.

An explicit `--blossom-server` list replaces `publication.blossom_servers`.
Explicit `--relay` values are added to and deduplicated with the manifest
relays. Boolean CLI flags and manifest values are enabling: if either is true,
the policy is enabled. Version, signer credentials, `--released-at`, `--tag`,
`--edit`, and output format remain runtime inputs so a committed manifest
cannot supply secrets, silently replace an event, or freeze per-release data.

Each asset has exactly one source:

- `file`: a repository-relative or absolute local path uploaded through
  Blossom; or
- `source`: an existing public HTTP(S) URL which ngit downloads and verifies
  before signing.

Asset metadata fields are:

- `platforms`: every platform supported by this asset;
- `platform_agnostic: true`: explicit alternative to `platforms`;
- `identifier` and `version`: asset identity overrides;
- `filename` and `mime`: published filename and MIME overrides;
- `min_platform_version` and `target_platform_version`;
- `supported_nips`;
- `variant`, `commit`, and `min_allowed_version`;
- `original_url`;
- `android`: Android package metadata.

An APK example is:

```yaml
schema: 1
application: com.example.my-app
assets:
  - file: artifacts/my-app-{version}.apk
    mime: application/vnd.android.package-archive
    android:
      version_code: 10203
      min_allowed_version_code: 10100
      certificate_sha256:
        - aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

The APK's platforms are inferred from the immutable snapshot. APKs cannot be
platform agnostic. `android.version_code` and at least one signing-certificate
SHA-256 are required.

Use `--manifest PATH` for a non-default manifest. Existing releases are never
replaced by creation; publication requires `--edit` when editing an addressable
release. See [the release API specification](release-api.md) for platform
policy, application ownership, JSON shapes, recovery behavior, and the complete
command reference. Default-manifest discovery is intentionally limited to
creation without direct asset flags; pass `--manifest` explicitly when mixing a
manifest with CLI assets or when editing.

## Using artifacts from ngit-ci

ngit-ci workflows live in `.ngit/act/workflows/` and use
GitHub Actions-compatible artifact actions. A build job can upload a named
artifact and a later job in the same workflow can download it to the path named
by `.ngit/release.yaml`:

```yaml
name: release
on:
  push:
    tags: ["v*"]

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          mkdir -p dist
          ./scripts/build-release dist/my-app
      - uses: actions/upload-artifact@v4
        with:
          name: release-binary
          path: dist/my-app

  release:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          name: release-binary
          path: artifacts
      - name: Publish NIP-82 release
        env:
          NOSTR_SECRET_KEY: ${{ secrets.NOSTR_SECRET_KEY }}
        run: |
          VERSION="${GITHUB_REF#refs/tags/v}"
          ngit --nsec "$NOSTR_SECRET_KEY" release publish "$VERSION" --json
```

This expects `.ngit/release.yaml` to use `file: artifacts/my-app`. The release
manifest may also own the ordered Blossom servers, relay targets, and stable
publication policy through its `publication` block, leaving the CI job to
supply only the version, signer, and output mode. The release image must provide
`ngit`, and the secret must belong to both a repository maintainer and the
Software Application author. Restrict release workflows to trusted maintainer
triggers; ngit-ci also withholds configured secrets from third-party pull
requests.

`upload-artifact` does not remove the original build output. When building and
publishing in one job, point `ngit release` directly at that original path and
omit `download-artifact`.

After a job completes, ngit-ci extracts each named artifact and uploads every
contained file as an individual Blossom blob. Its kind-9841 Job Result contains:

```jsonc
["artifact", "<blossom-url>", "<path-within-artifact>", "<artifact-name>"]
```

Those tags are published after the workflow has finished, so they are not a
same-run input. For a completed run, an exact trusted Job Result can be
inspected and its URL passed to `--asset PLATFORM=URL`; avoid selecting an
artifact only by name or by “latest”, because names repeat across runs and
untrusted providers can publish claims for the same repository and commit.

ngit-ci cannot currently pass artifacts across independent workflow files or
runs. In particular, architectures built by separate coordinators cannot fan
into a release job with `needs`; either build the release set in one supported
workflow invocation or assemble it later from exact, trusted result events.
