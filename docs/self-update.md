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

The installers always use the exact pinned release archive directly. They do
not call an installed `ngit update`, so installation and repair work even when
the old CLI lacks that command, its helper is missing, or its relay discovery
is unavailable. Both binaries are validated in a staging directory before
replacement. On ordinary replacement errors, the previous binaries and receipt
are restored. An installation lock excludes concurrent installers.

### Existing installations and Cargo choices

A valid standalone receipt allows replacement in the existing directory.
Executable and directory symlinks are resolved when finding that installation.
For an existing Cargo installation, the installer automatically announces and
runs a Cargo upgrade, preserving the detected root. This works even when the
old ngit has no `update` command:

```bash
curl -fsSL https://ngit.dev/install.sh | bash
```

`--method cargo` explicitly selects the same behaviour. If Cargo is unavailable
or fails, the installer reports the failure; it does not switch methods.
Other installations without a receipt still require an explicit choice to
install standalone separately. The direct-download path never falls back into
`~/.cargo/bin` or overwrites unreceipted files in the selected destination.

To switch to a standalone copy that supports `ngit update`:

```bash
curl -fsSL https://ngit.dev/install.sh | bash -s -- --method standalone
```

The standalone copy normally goes into `~/.local/bin`. The Cargo binaries and
Cargo's installation records stay intact. Follow the printed PATH command to
make the standalone copy active, and keep that setting in your shell startup
file. The installer checks both `ngit` and `git-remote-nostr`; either can be
masked by an older copy. Removing the Cargo copy later with `cargo uninstall
ngit` is optional (reuse its custom `--root`, if applicable).

Use `--install-dir /absolute/path` to choose a different standalone directory.
An existing unreceipted executable in that directory is never overwritten;
choose an empty directory or update through its original installation tool.
This also applies to package-manager and source-built installations.

### Repair and version policy

Rerun the installer to repair a missing helper or reinstall the pinned stable
release. For an invalid standalone receipt, use `--repair`; combine it with
`--install-dir` when the intended installation is not the active `ngit` on PATH.
A missing receipt does not authorize replacement of existing binaries: choose
a separate directory. Unknown receipt schemas require explicit repair, too.

Replacing a newer release or an executable whose version cannot be read
requires `--allow-downgrade`. This also applies when switching from a newer
prerelease to the website's older stable release. A prerelease of the same
major/minor/patch version can be replaced by the final stable release. The
installer does not silently downgrade an inactive copy in the chosen directory.

If the process is forcibly killed or rollback itself fails, preserve the
`.ngit-install.*` (Unix) or `.ngit-install-*` (Windows) staging directory and
its `old` backups. Restore the originals
before removing a leftover `.ngit-install-lock` and retrying. Normal errors and
handled termination signals clean up automatically; power-loss recovery is
manual.

On NixOS the default standalone path shows a stable-branch `nix profile add`
command using ngit.dev's GRASP-backed Git alias, then exits without changing the
profile when no Cargo installation was selected. `--standalone` explicitly
selects the static musl archive for a user-owned x86_64 installation. Other NixOS architectures remain Nix-managed
until a matching static asset is published. Existing Cargo installations also
update through Cargo automatically on NixOS; `--method cargo` selects that
method explicitly. Neither installer path adopts files in the Nix store.

### Windows

Save the pinned `install.ps1` from the installation page and run it normally
to upgrade an existing Cargo installation through Cargo automatically. Use
`-Method standalone` to switch to a separate downloaded copy, or `-Method cargo`
to select Cargo explicitly. The matching options are `-InstallDirectory`,
`-Repair`, and `-AllowDowngrade`. The default standalone destination is
`%LOCALAPPDATA%\Programs\ngit\bin`. The installer validates and stages both
binaries, restores originals on replacement failure, and moves its directory
to the front of the user PATH. Restart the terminal afterward. An earlier
system PATH entry can still mask it; warnings identify the affected commands.
Close running ngit processes before replacement if Windows reports a locked
file. Preserve the printed backup directory if restoration is blocked too.
Windows standalone updates currently require rerunning this installer;
`ngit update` reports that native replacement is unsupported.

### Installer regression tests

`cargo test --test installer_templates` uses temporary directories, fixture
archives, fake executables, and subprocess-only environment settings. It does
not contact release servers, invoke real Cargo/ngit installations, or modify
user profiles. Unix tests expose only the required shell tools on PATH and
exercise the real download verification, extraction, selection, and replacement
logic. PowerShell transaction tests run when `pwsh` is available; otherwise
that portion is skipped explicitly. On Nix, run the full installer checks with:

```bash
nix shell nixpkgs#powershell --command cargo test --test installer_templates
```

The PowerShell tests use version fixtures, so they run on Unix as well as
Windows without executing platform-specific release binaries. Actual Windows
file-lock and terminal behaviour still require Windows verification.
