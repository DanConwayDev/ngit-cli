# ngit updates and standalone installation

ngit separates cheap update discovery from release validation and installation.

Normal repository traffic may cache ngit's single kind-30618 repository-state
event. A newer eligible Git tag is only a candidate: tag pushes trigger the CI
jobs which build release archives, upload them to Blossom, and publish the
corresponding NIP-82 application, asset, and release events. ngit therefore
does not warn until the matching main-channel release and all of its referenced
assets have also reached the cache and pass strict validation.

Stable ngit builds ignore prerelease tags. A prerelease build may follow newer
prereleases and the eventual stable release. Cached discovery adds only exact
filters for the candidate release identifier (`ngit@VERSION`, with legacy
support for `ngit@vVERSION`) and its immutable asset event IDs; it never lists
the complete release history.

## `ngit update`

`ngit update --check` refreshes the state hint, queries the one exact
NIP-82 release, resolves its referenced assets, and reports one of:

- `current`: no eligible newer tag exists;
- `pending`: the tag exists but CI has not published a complete installable
  release yet;
- `available`: the signed release contains a unique asset for this platform.

`ngit update VERSION` selects one exact version from the signed state instead
of choosing the newest eligible tag. This permits intentional prerelease and
older-version installs without treating prerelease status itself as a problem.
If the selected version is behind a newer eligible tag, ngit prints a note.
Stable automatic checks continue to ignore prerelease tags.

Without `--check`, ngit automatically replaces only installations bearing the
standalone receipt written by the official bootstrap installer. The updater
canonicalizes its own executable before classifying ownership. It never writes
into `/nix/store`, invokes `sudo`, or adopts an unreceipted Cargo, package
manager, or source installation. Those installations receive non-mutating,
actionable guidance instead.

For a standalone update, ngit downloads the asset URL from the signed NIP-82
event, verifies its SHA-256 and size, extracts both `ngit` and
`git-remote-nostr`, executes their version checks, and stages rollback copies
before replacing either binary.

## Website installer handoff

[`install/install.sh.in`](../install/install.sh.in) and
[`install/install.ps1.in`](../install/install.ps1.in) are the canonical Unix
and Windows bootstrap templates. They are deliberately pinned and contain two
deployment placeholders:

- `@@VERSION@@`: the exact stable NIP-82 release version;
- `@@ASSET_MANIFEST@@`: newline-separated
  `target|url|sha256|filename|mime` records.

The ngit-dev-website deployment must obtain the release with
`ngit release view --json`, reject anything other than a valid `main` release,
and render exactly one record for each supported installer target:

| Installer target | Required NIP-82 platform and variant |
| --- | --- |
| `linux-x86_64-gnu` | `linux-x86_64`, variant containing `gnu` or `glibc` |
| `linux-x86_64-musl` | `linux-x86_64`, variant containing `musl` |
| `linux-aarch64-gnu` | `linux-aarch64`, variant containing `gnu` or `glibc` |
| `darwin-universal` | one asset covering both `darwin-x86_64` and `darwin-aarch64` |
| `windows-x86_64` | `windows-x86_64` |

Every rendered URL must be a content-addressed HTTPS Blossom URL from its
kind-3063 asset; the rendered hash, filename, and MIME type must be copied from
the same signed event. Deployment must fail on missing, duplicate,
unreferenced, or invalid assets. The generated `/install.sh` and
`/install.ps1` are updated only after all release events and Blossom placements
are queryable.

The shell installer uses `ngit update` only when the existing installation has
a standalone installer receipt, reports v3 or later, and accepts
`ngit update --help`. This keeps native release validation and rollback for
supported standalone installations.

For older versions without `ngit update`, the script skips that command and
downloads the pinned release archive directly. It replaces both `ngit` and
`git-remote-nostr` in the existing standalone directory when that directory has
an installer receipt, is writable, and the `ngit` executable is not a symlink.

An old Cargo installation normally has no standalone receipt. In that case,
the script also skips `ngit update`, but selects a standalone installation
directory, normally `~/.local/bin`, rather than upgrading through Cargo. The old
Cargo binary can therefore remain on disk and take precedence on PATH. The
script reports its destination and, if needed, explains how to put that
directory first on PATH and identifies the binary taking precedence. To
replace the Cargo installation in place instead, run `cargo install ngit --locked`.

On NixOS it shows a stable-branch `nix profile add` command using ngit.dev's
GRASP-backed Git alias, then exits without changing the profile. `--standalone`
is an explicit escape hatch which selects the static musl archive for a
user-owned x86_64 installation. Other NixOS architectures remain Nix-managed
until a matching static asset is published.
