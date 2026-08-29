# ngit

nostr plugin for git

- clone a nostr repository, or add as a remote, by using the url format nostr://<npub123|nip05-address>/<identifier>
- remote branches beginning with `pr/` are open PRs from contributors; `ngit list` can be used to view all PRs
- to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options
- publish a repository to nostr with `ngit init`

browse [gitworkshop.dev/repos](https://gitworkshop.dev/repos) to find nostr repositories.

## install

install options:

1. live on the edge with one-line install: `curl -Ls https://ngit.dev/install.sh | bash`
2. **build from source**: clone this repository, [install rust and cargo](https://www.rust-lang.org/tools/install), checkout the latest release tag, run `cargo build --release` and move `./target/release/ngit` and `./target/release/git-remote-nostr` to your PATH.
3. **install with cargo**: [install rust and cargo](https://www.rust-lang.org/tools/install), run `cargo install ngit`, maken sure `~/.cargo/bin` is in your PATH
4. **install with nix**: add `ngit.url = "github:DanConwayDev/ngit-cli";` as a flake input and then include `inputs.ngit.packages."${pkgs.system}".default` in packages.
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
git config nostr.auto-pr-branches false     # fetch PR branches only after `ngit pr checkout`
git config nostr.http-io-timeout-ms 600000  # allow large GRASP pushes up to 10 minutes of socket silence
```

`nostr.auto-pr-branches` defaults to `true` and follows normal Git config
precedence, so repository-local config overrides global config. Set it to
`false` to avoid advertising and downloading every open or draft PR branch;
add `--global` to make that the default for all repositories. Running
`ngit pr checkout <id>` opts that PR branch back in and configures it for later
`git fetch` and `git pull`. Run `git fetch --prune` once to remove any PR
branches fetched before opting out. To override a global setting during the
initial clone, use `git clone --config nostr.auto-pr-branches=true <nostr-url>`.

Set `NGIT_CACHE_DIR` to place ngit's global event cache in a different
writable directory, for example in a sandbox or ephemeral agent environment.
If that directory is unavailable, ngit uses an in-memory global cache for the
current process. Repository event caches remain in the Git common directory;
commands that use them require that directory (normally `.git`) to be writable.

Secrets are kept in the OS credential store where possible; see
[docs/credential-storage.md](docs/credential-storage.md) for the storage
model, plaintext migration, and how to opt out.

See [publishing releases](docs/releases.md) for local files, release manifests,
multi-platform assets, and ngit-ci artifact workflows.

See [publishing static sites](docs/nsites.md) for deploying an already-built
directory with `ngit nsite publish` through Blossom and NIP-5A.

See [publishing containers](docs/containers.md) to upload an OCI image layout
to Blossom and publish its signed kind-30624 tag map with `ngit container`
(`ngit oci`).

## contributions welcome!

[gitworkshop.dev/danconwaydev.com/ngit](https://gitworkshop.dev/danconwaydev.com/ngit) to report issues and see PRs

use ngit to submit PRs with clone url: `nostr://danconwaydev.com/relay.ngit.dev/ngit`

## primer

nostr is a decentralised communications protocol with:

- permissionless account creation - created via a public/private key pair
- verifiable signed messages
- messages transported via relays rather than P2P

for code collaboration, nostr is used for:

- repository identification and discovery
- state (ie. git refs)
- proposals (PRs), issues and related discussion

a git server is still required for data storage and syncing state. multiple git servers can be used for reduncancy and they can be seemlessly swapped out by maintainers just like nostr relays. see [maintainer model](docs/architecture/maintainer-model.md) for details on how multi-maintainer repositories work.

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
was removed after installation. Install and upgrade create a dedicated commit;
contributors who are not maintainers should push it from a `pr/` branch to
propose the change as a pull request.

Run `ngit skill --help` for status and reminder opt-out commands.
