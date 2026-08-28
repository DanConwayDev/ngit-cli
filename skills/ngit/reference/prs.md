# Pull Requests — open, stack, review, merge

Part of the ngit skill. Read this before opening, reviewing, or merging pull requests.

## Open a PR

> **CRITICAL: Branch name MUST start with `pr/`** — this is what signals ngit to create a PR. A branch without the `pr/` prefix is a plain push and will NEVER create a PR, regardless of push options.

```bash
git checkout -b pr/my-feature          # MUST use pr/ prefix — not "my-feature", not "feature/foo"
# ... commits ...

# Single commit: omit title/description — commit subject and body are used automatically (preferred)
git push -u origin pr/my-feature

# Multiple commits: supply title and description explicitly
# Use this only for short inline descriptions. Write the two literal characters \n
# for each line break; ngit's push-option parser converts them to real newlines.
# Do NOT use $'...\n\n...' ANSI-C quoting — git cannot pass real newlines through push options.
git push -u origin pr/my-feature \
  -o 'title=My feature title' \
  -o 'description=First paragraph.\n\nSecond paragraph.'

# Target a non-default branch
git push -u origin pr/release-fix -o target-branch=release/2.x

# Stacks are inferred when this branch contains the unique latest tip of one
# of your other open or draft PRs. Override the current publication for an
# ambiguous stack, a cross-author parent, or a deliberately historical parent.
git push -u origin pr/second-part -o base=<commit|branch|nevent>

# An inferred child follows its parent's latest update after you rebase it.
# Use base= only to override or pin that inference.
git push --force origin pr/second-part -o base=<commit|branch|nevent>
```

When there is only one commit, omitting `-o title=` and `-o description=` is preferred — ngit uses the commit subject as the title and the commit body as the description. Pass `-d` (or `--defaults`) to confirm this automatically. `git push` or `git push --force` can update existing PRs (branch must still have the `pr/` prefix).

`--signer` applies to direct `ngit` commands, not to `git push`. To publish one
push as another stored identity without changing the configured login, use
`git -c nostr.signer=<alias|npub|nostr-display-name> push ...`. To make the
identity the repository default instead, use
`ngit account login --local --alias <alias>` or set `nostr.signer` in local Git
config. Keep using `--signer` for direct follow-up actions such as comments,
labels, and lifecycle changes when they should use a non-default identity.

**Do not generate a `git push -o description=...` value from a Markdown file.**
This restriction is specific to Git push options, which cannot contain real
newlines. Pre-escaping a file with `perl`, `sed`, `string join`, or similar
introduces multiple layers of shell and git escaping.
Normal `ngit` options such as `--body` and `--description` do accept multiline
arguments from a quoted `"$(cat file.md)"`. To open a proposal with an existing
description file, use `ngit send`; do not also push a new `pr/` branch for the
same proposal.

## Advanced: ngit send

Like other `ngit` text options, `ngit send --description` takes a regular shell
argument and therefore accepts real newlines. The shell does **not** interpret
`\n` inside double-quoted strings, so `"...\n\n..."` produces literal
backslash-n in the event. Use ANSI-C quoting (`$'...'`) for inline multiline
text, or a quoted command substitution for a file:

```bash
# correct — $'...' quoting gives real newlines
ngit send HEAD~2 \
  --subject "My Feature" \
  --description $'First paragraph.\n\nSecond paragraph.' \
  --json

# Existing Markdown file (POSIX shells such as bash and zsh): quote the
# substitution so the complete file is passed as one argument with real newlines.
ngit send HEAD~2 \
  --subject "My Feature" \
  --description "$(cat .git/pr-description.md)" \
  --json

# WRONG — \n inside double quotes is not interpreted; event contains literal \n\n
ngit send HEAD~2 --subject "My Feature" --description "First paragraph.\n\nSecond paragraph."

ngit send --defaults --json                             # non-interactive
ngit send HEAD~2 --in-reply-to <PR-event-id> --json    # update existing PR
ngit send --defaults --target-branch release/2.x --json # target a non-default branch
ngit send --defaults --base <commit|branch|nevent> --json # override this publication's inference
ngit send --defaults --in-reply-to <PR-event-id> \
  --base <commit|branch|nevent> --json                   # override an inferred parent
```

Both `git push` and `ngit send` automatically use the unique most-advanced tip
of your other open or draft PRs when it is in the proposal's history and ahead
of the target branch. An existing child remembers that parent lineage: after
the parent advances, rebase the child onto its latest tip before updating it.
ngit refuses stale children and unrelated ambiguous candidates instead of
guessing. `--base` / `-o base=` is therefore optional for ordinary same-author
stacks, but remains the explicit pin for cross-author, historical, or ambiguous
cases. Repeat an explicit historical base on each later child update if the
child should remain pinned there; otherwise the open parent lineage advances
automatically.

## List / view / comment

```bash
ngit pr list --json
ngit pr list --json --status open,draft,closed,applied
ngit pr list --json --label bug
ngit pr view <ID|nevent> --json
ngit pr view <ID|nevent> --json --comments
ngit pr comment <ID|nevent> --body "Looks good" --json
ngit pr comment <ID|nevent> --body "Fixed!" --reply-to <comment-ID|nevent> --json
```

## Checkout / apply

```bash
ngit pr checkout <ID|nevent> --json
```

## Merge (maintainer)

```bash
ngit merge <ID|nevent> --require-ci-trust maintainer-directed --json # gate on green trusted CI
ngit pr checkout <ID|nevent> --json
ngit merge --require-ci-trust maintainer-directed --json # infer PR from checked-out pr/ branch
ngit merge --exclude-description <ID|nevent> --json
git push origin <target-branch>           # publishes the merge and applied status
```

`ngit merge` creates a no-ff merge commit on the PR's indexed `b` target, or on
the repository default when the PR has no explicit target, with the standard
`Merge #<8-hex>: <PR title>` message. It resolves an explicit target against
the latest Nostr repository state, so a stale local tracking ref cannot route
the merge onto old history. If conflicts occur, resolve them and run
`git commit`; ngit has already prepared the commit message.

Before adding maintainer fixes or merging, inspect PR-only merge commits with
`git log --merges --oneline origin/<target>..HEAD`. If it shows a prior
`Merge #...`, stop: `ngit merge` would create nested merge history. Unless that
history is intentional, rebase or cherry-pick the PR commits onto the current
target branch before updating the PR.

## Lifecycle

```bash
ngit pr close <ID|nevent> --reason "blocked by upstream" --json
ngit pr reopen <ID|nevent> --reason "fix was incomplete" --json
ngit pr ready <ID|nevent> --reason "addressed review feedback" --json
ngit pr draft <ID|nevent> --reason "needs more work" --json
ngit pr label <ID|nevent> --label bug --label enhancement --json
ngit pr set-subject <ID|nevent> --subject "New title" --json
ngit pr set-cover-note <ID|nevent> --body "Updated description. See nostr:nevent1abc…" --json
```
