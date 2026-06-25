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

run the commands `ngit` and `git-remote-nostr` to ensure the binaries are in your PATH.

## configuration

Run `ngit --customize` to list supported git config keys and their environment-variable overrides. Useful examples:

```sh
git config nostr.repo-relay-only true       # only publish nostr events to repo relays
git config nostr.http-io-timeout-ms 600000 # allow large GRASP pushes up to 10 minutes of socket silence
```

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
