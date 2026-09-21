# Choosing the commits in a proposal

A proposal normally compares your branch with the Nostr destination's published
default branch. Your fork's default branch may already contain your changes;
that does not mean they have been accepted by the destination. Remote names
such as `origin` and `github`, and branch tracking configuration, do not establish
ownership or accepted history. ngit does not automatically trust their tips.

## Choosing an explicit boundary

When the intended proposal boundary differs from the Nostr destination's tip,
choose it explicitly with `--base`. This means "exclude history through this
commit"; it does not ask ngit to establish who owns a remote. For example, to
propose the last three commits on a linear branch:

```sh
ngit --repo upstream send --base HEAD~3 --defaults
```

You can use a commit ID or a ref you have checked instead. Suppose you know that
`github/master` identifies the accepted history you built on, and want to exclude
it even though Nostr has not caught up. Inspect that boundary and select it:

```sh
git fetch github
git log github/master..HEAD
ngit --repo upstream send --base github/master --defaults
```

Here `upstream` names your `nostr://` destination and `github/master` is only
an example ref: it could belong to your fork, a maintainer, or someone else.
Use the ref that identifies your intended base, not one selected by its name.
The base must be an ancestor of your current commit.

If you prefer to publish with Git, use this alternative from your PR branch:

```sh
git push upstream pr/feature -o base=github/master
```

Choose one publication method for a new proposal. An explicit base works for
contributors as well as maintainers and needs no `--force` for this case. Repeat
the base option when you need to keep that boundary on later publications.
These commands publish a proposal; they do not advance the destination's master.

The syntax for three commits back is `HEAD~3`, not `~HEAD-3`. You can use
`ngit send HEAD~3` to select the last three commits on a linear branch. A range
that starts ahead of the destination can require confirmation (`--force` in
noninteractive mode). `--base <commit-or-ref>` makes the intended exclusion
explicit; using a verified ref or commit ID avoids relying on a commit count.

## Maintainers and local default branches

A confirmed maintainer's local default can contain accepted upstream changes
learned through another publishing remote. It can be a base candidate when it
extends the destination's default, regardless of its tracking remote.

- If the PR tip equals local default, all commits ahead of the destination are
  proposed. There is no additional `--force` requirement.
- If using local default would exclude unpublished commits shared with a new
  PR, ngit asks for `git push --force` to confirm using that local base. Those
  commits remain in Git history but are excluded from the proposed change.
- Local advances absent from the PR's history do not require that confirmation.
- Use `-o base=<commit-or-ref>` to select a different boundary explicitly.

Contributors' local defaults do not advance the automatic base. Use the explicit
base options above when you intend to exclude additional history.

## Drafting directly on master

`ngit send` supports selecting draft commits directly on the default branch,
including for maintainers. For example, `ngit send HEAD~3 --defaults` selects
three commits on a linear branch; `ngit send --defaults` selects the automatically
suggested commits. The parent of the first selected commit becomes the proposal
base. When selecting a specific upstream boundary, use `--base` as above.
