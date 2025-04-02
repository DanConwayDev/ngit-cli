# ngit

nostr plugin for git

- clone a nostr repository, or add as a remote, by using the url format nostr://<pub123|nip05-address>/<identifier>
- remote branches beginning with `pr/` are open PRs from contributors; `ngit list` can be used to view all PRs
- to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options
- publish a repository to nostr with `ngit init`

browse [gitworkshop.dev/repos](https://gitworkshop.dev/repos) to find nostr repositories.

## install

install options:

1. **build from source**: clone this repository, [install rust and cargo](https://www.rust-lang.org/tools/install), checkout the latest release tag, run `cargo build --release` and move `./target/release/ngit` and `./target/release/git-remote-nostr` to your PATH.
2. **install with cargo**: [install rust and cargo](https://www.rust-lang.org/tools/install), run `cargo install ngit`, maken sure `~/.cargo/bin` is in your PATH
3. **install with nix**: add `ngit.url = "github:DanConwayDev/ngit-cli";` as a flake input and then include `inputs.ngit.packages."${pkgs.system}".default` in packages.
4. download the latest release binaries from [gitworkshop.dev/ngit](https://gitworkshop.dev/ngit) and add to PATH

run the commands `ngit` and `git-remote-nostr` to ensure the binaries are in your PATH.

## contributions welcome!

[gitworkshop.dev/repos/ngit](gitworkshop.dev/r/naddr1qqzxuemfwsq3gamnwvaz7tmjv4kxz7fwv3sk6atn9e5k7q3q5qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exsxpqqqpmejawq4qj) to report issues and see PRs

use ngit to submit PRs with clone url: `nostr://dan@gitworkshop.dev/relay.damus.io/ngit`

## primer

nostr is a decentralised communications protocol with:

- permissionless account creation - created via a public/private key pair
- verifiable signed messages
- messages transported via relays rather than P2P

for code collaboration, nostr is used for:

- repository identification and discovery
- state (ie. git refs)
- proposals (PRs), issues and related discussion

a git server is still required for data storage and syncing state. multiple git servers can be used for reduncancy and they can be seemlessly swapped out by maintainers just like nostr relays.

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
