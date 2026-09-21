# Choosing the commits in a proposal

A proposal normally compares your branch with the Nostr destination's published
default branch. Your fork's default branch may already contain your changes;
that does not mean they have been accepted by the destination.

## When another upstream remote is ahead of Nostr

Suppose you pulled accepted changes from the maintainer's GitHub repository,
then added three commits of your own. The maintainer has not yet published those
accepted changes to Nostr. Name the GitHub branch as your base so the proposal
contains only your work:

```sh
git fetch github
ngit --repo upstream send --base github/master --defaults
```

Here `upstream` is your `nostr://` remote and `github` is the maintainer's other
publishing remote. Use `github/main` instead if that is its default branch.
The base must be an ancestor of your current commit. Inspect the selected work
with `git log github/master..HEAD` before sending.

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
noninteractive mode). Prefer `--base github/master` here: it states why those
earlier upstream commits are excluded without relying on a commit count.

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
base options above when you know another upstream contains accepted history.

## Drafting directly on master

`ngit send` supports selecting draft commits directly on the default branch,
including for maintainers. For example, `ngit send HEAD~3 --defaults` selects
three commits on a linear branch; `ngit send --defaults` selects the automatically
suggested commits. The parent of the first selected commit becomes the proposal
base. When selecting a specific upstream boundary, use `--base` as above.
