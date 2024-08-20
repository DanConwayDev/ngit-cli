# ngit

a command-line tool to send and review patches via nostr

- works seemlessly with [gitworkshop.dev](https://gitworkshop.dev)
- fully implements nostr git protocol (nip34)
- enables proposals to be managed as branches, similar to GitHub PRs, or patches similar to patches-over-email

see [gitworkshop.dev/ngit](https://gitworkshop.dev/ngit) and [gitworkshop.dev/about](https://gitworkshop.dev/about) for more details

## git-remote-nostr

a git remote helper (git plugin) included with ngit that enables nostr integration with native git commands when used with a nostr remote eg nostr://npub123/identifer

- repository state stored in a nostr event with git server(s) used for data sync
- treats open proposals branches prefixed `pr/*`

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

### ngit commands

run from the directory of the git repository:

- `ngit init` signal you are this repo's maintainer accepting proposals via nostr
- `ngit send` issue commits as a proposal
- `ngit list` list proposals; checkout, apply or donwload selected
- `ngit fetch` fetch download latest repository updates to allow `ngit list` usage offline

and when on a proposal branch:

- `ngit push` send proposal revision
- `ngit pull` fetch and apply new proposal commits / revisions linked to branch

## contributions welcome!

use ngit to submit proposals!

[gitworkshop.dev/r/naddr1qqzxuemfwsq3gamnwvaz7tmjv4kxz7fwv3sk6atn9e5k7q3q5qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exsxpqqqpmejawq4qj](https://gitworkshop.dev/repo/ngit) to report issues and see proposals

install the tool with `cargo install ngit`, use a prebuilt binary or build from source off the master branch.
