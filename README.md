# ngit

nostr plugin for git

- clone a nostr repository, or add it as a remote, with `nostr://<npub|nip05-address>/<identifier>` or a [NIP-AD](https://github.com/nostr-protocol/nips/pull/2406) web address such as `nostr://ngit.dev/ngit.git`
- remote branches beginning with `pr/` are open PRs from contributors; `ngit list` can be used to view all PRs
- to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options
- publish a repository to nostr with `ngit init`

browse [gitworkshop.dev/repos](https://gitworkshop.dev/repos) to find nostr repositories.

## install

install options:

1. live on the edge with one-line install: `curl -Ls https://ngit.dev/install.sh | bash`
2. **build from source**: clone this repository, [install rust and cargo](https://www.rust-lang.org/tools/install), checkout the latest release tag, run `cargo build --release` and move `./target/release/ngit` and `./target/release/git-remote-nostr` to your PATH.
3. **install with cargo**: [install rust and cargo](https://www.rust-lang.org/tools/install), run `cargo install ngit`, maken sure `~/.cargo/bin` is in your PATH
4. **install with nix**: run `nix profile add 'git+https://ngit.dev/ngit.git?ref=stable'`. the `stable` branch always points at the latest stable release.
5. download the latest release binaries from [gitworkshop.dev/ngit](https://gitworkshop.dev/ngit) and add to PATH

run the commands `ngit` and `git-remote-nostr` to ensure the binaries are in your PATH. `git-remote-nostr` is a small compatibility launcher that git discovers by name; the implementation lives in `ngit`, so both need installing together.

### trusting locally installed certificate authorities

Relay connections in release binaries use the self-contained WebPKI root
store. To additionally trust certificate authorities installed on the local
system, such as an `mkcert` authority used by a development grasp server, build
ngit with the `native-tls-roots` feature:

```sh
cargo build --release --features native-tls-roots
```

The feature can also be enabled when installing from crates.io:

```sh
cargo install ngit --features native-tls-roots
```

This expands which certificate authorities ngit trusts and can vary by
platform, so enable it only when the system trust store is appropriate for the
environment. WebPKI roots remain enabled alongside the native roots. This
changes the relay TLS root stores, not the Rustls cryptographic provider; Ring
remains the sole provider. Other HTTPS transports use their own platform
verification behavior.

## configuration

Run `ngit --customize` to list supported git config keys and their environment-variable overrides. Useful examples:

```sh
git config nostr.repo-relay-only true       # only publish nostr events to repo relays
git config nostr.auto-pr-branches true      # fetch every open and draft PR branch
git config nostr.http-io-timeout-ms 600000  # allow large GRASP pushes up to 10 minutes of socket silence
```

`nostr.auto-pr-branches` defaults to `false` and follows normal Git config
precedence, so repository-local config overrides global config. Open and draft
PRs from other users are not advertised or downloaded as branches during
routine Git operations. Running `ngit pr checkout <id>` creates the familiar
`pr/<branch-name>(<shorthand-id>)` local branch and configures it for later
`git fetch`, `git pull`, and `git push`. Set the option to `true` (and add
`--global` if desired) to restore automatic PR branches. Run
`git fetch --prune` once to remove branches fetched by an older ngit version;
unset config uses the new default in both fresh and existing clones.
To enable automatic PR branches during the initial clone, use
`git clone --config nostr.auto-pr-branches=true <nostr-url>`.

Set `NGIT_CACHE_DIR` to place ngit's global event cache in a different
writable directory, for example in a sandbox or ephemeral agent environment.
If that directory is unavailable, ngit uses an in-memory global cache for the
current process. Repository event caches remain in the Git common directory;
commands that use them require that directory (normally `.git`) to be writable.
Outgoing events enter these caches only after a relay accepts them (or confirms
it already has them). Undelivered edits do not change cached state.

Secrets are kept in the OS credential store where possible; see
[docs/credential-storage.md](docs/credential-storage.md) for the storage
model, plaintext migration, and how to opt out.

See [publishing releases](docs/releases.md) for local files, release manifests,
multi-platform assets, and ngit-ci artifact workflows.

ngit checks trusted release readiness without listing the complete release
history. Standalone installations can run `ngit update`; Nix, Cargo, and
other package-managed installations are detected and left untouched. See
[ngit updates and standalone installation](docs/self-update.md) for the trust,
CI-delay, and website-installer contracts.

See [publishing static sites](docs/nsites.md) for deploying an already-built
directory with `ngit nsite publish` through Blossom and NIP-5A.

See [publishing containers](docs/containers.md) to upload an OCI image layout
to Blossom and publish its signed kind-30624 tag map with `ngit container`
(`ngit oci`).

## contributions welcome!

[gitworkshop.dev/danconwaydev.com/ngit](https://gitworkshop.dev/danconwaydev.com/ngit) to report issues and see PRs

use ngit to submit PRs with clone url: `nostr://danconwaydev.com/ngit`

## primer

nostr is a decentralised communications protocol with:

- permissionless account creation - created via a public/private key pair
- verifiable signed messages
- messages transported via relays rather than P2P

for code collaboration, nostr is used for:

- repository identification and discovery
- state (ie. git refs)
- proposals (PRs), issues and related discussion

a git server is still required for data storage and syncing state. multiple git servers can be used for reduncancy and they can be seemlessly swapped out by maintainers just like nostr relays. see the [maintainer guide](https://ngit.dev/maintainers) for details on how multi-maintainer repositories work.

eg self-hosted, github, codeberg, etc.

```
             ┌──────────┐
             │  Author  │
             └──/─┬─\───┘
        ,------'  │  '--------.-------.
┌──────▼─┐   ┌────▼───┐   ┌───▼───┐  ┌─▼─────┐  ┌───────┐
│  Git   │   │  Git   │   │ Relay │  │ Relay │  │ Relay │
│ Server │   │ Server │   │       │  │       │  │       │
└────────┘   └────\───┘   └───┬───┘  └──/────┘  └─/─────┘
                   \------.   │   ,----/---------/
                         ┌─▼──▼──▼─┐
                         │  User   │
                         └─────────┘
```

## Repository-managed skill

Run `ngit skill install` to add repository guidance that teaches coding agents
to use ngit. The command installs the skill in the Codex and Claude discovery
locations and adds a compact pointer to existing `AGENTS.md` or `CLAUDE.md`
files without creating either file. Use `ngit skill upgrade` to update existing
copies from the version in the skill metadata; it does not restore a copy that
was removed after installation. Install and upgrade leave their changes
uncommitted so the repository's normal validation and commit workflow can run.
Contributors who are not maintainers should commit the changes on a `pr/`
branch and push it to propose a pull request. With `--json`, `changed_files`
lists the managed paths that remain as Git changes and `changes_uncommitted`
states whether there is anything to commit. Git-ignored managed files are
installed but omitted from those fields because ordinary Git staging excludes
them. Successful commands set `action` to `installed`, `upgraded`,
`reconciled`, or `unchanged`; partial update failures use `failed`, report the
same change fields for files already left in the worktree, and omit post-update
guidance status fields.

Run `ngit skill --help` for status and reminder opt-out commands.
