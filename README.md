# ngit

cli for code collaboration over nostr

It supports both:

* patches over nostr similar to `git format-patch` and `git send-email` following the [nip34 daft spec](https://github.com/nostr-protocol/nips/pull/997)
* branches similar to pull request model popularised by github

so that users can decide to work with either model using the same nost events.

the term 'proposals' is used to bridge the divide between 'patches and patch sets over email' and 'PRs'

patches produced using other nip34 clients will work with the nip34 patch model but wont have branch support.


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

install the tool with `cargo install ngit`, use a prebuilt binary or build from source off the master branch.