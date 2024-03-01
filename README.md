# ngit

a command-line tool to send and review patches via nostr

* works seemlessly with [gitworkshop.dev](https://gitworkshop.dev)
* fully compatible with [nip34 draft](https://github.com/nostr-protocol/nips/pull/997)
* enables proposals to be managed as branches, similar to GitHub PRs via nip34+

see [gitworkshop.dev/ngit](https://gitworkshop.dev/ngit) and [gitworkshop.dev/about](https://gitworkshop.dev/about) for more details

### Commands

run from the directory of the git repository:

* `ngit init` signal you are this repo's maintainer accepting proposals via nostr
* `ngit send` issue commits as a proposal

* `ngit list` list proposals; checkout, apply or donwload selected

and when on a proposal branch:

* `ngit push` send proposal revision

* `ngit pull` fetch and apply new proposal commits / revisions linked to branch

## Contributions Welcome!

use ngit to submit proposals!

[gitworkshop.dev/repo/ngit](https://gitworkshop.dev/repo/ngit) to report issues and see proposals

install the tool with `cargo install ngit`, use a prebuilt binary or build from source off the master branch.