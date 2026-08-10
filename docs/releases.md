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

The file is snapshotted once, uploaded once to the primary Blossom server, and
represented by one asset event with two `f` platform tags. On a repository with
no existing NIP-82 events, the same command creates and publishes the linked
application, asset, and release in dependency order.

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
application: com.example.my-app
channel: main
notes: Maintenance and compatibility improvements.
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
- `channel`: defaults to `main` on creation;
- `notes`: release notes;
- `assets`: one or more asset objects.

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
command reference.

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
image must provide `ngit`, and the secret must belong to both a repository
maintainer and the Software Application author. Restrict release workflows to
trusted maintainer triggers; ngit-ci also withholds configured secrets from
third-party pull requests.

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
