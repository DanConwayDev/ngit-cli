---
name: ngit
description: Use when working with nostr:// git repositories, submitting or reviewing pull requests (PRs) or patches over Nostr, creating or viewing git Nostr issues, cloning a nostr:// URL, publishing a repo to Nostr with ngit init, or any task involving the ngit CLI or git-remote-nostr. Triggers include: "ngit", "nostr:// repo", "git nostr", "submit a PR", "submit a patch", "open an issue on nostr", "clone nostr://", "view nostr issues", "ngit init", "push pr/ branch".
license: CC-BY-SA-4.0
---

# ngit — Nostr Plugin for Git

ngit makes native git commands (`clone`, `fetch`, `push`) work with `nostr://` URLs and adds a CLI for pull requests, issues, and repository management — all over the decentralised Nostr protocol. No GitHub, no centralised forge.

- Homepage: https://ngit.dev
- Source: `nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit`
- Web UI for browsing repos/PRs/issues: https://gitworkshop.dev

## Installation

```bash
curl -Ls https://ngit.dev/install.sh | bash
```

This installs two binaries:

- `ngit` — the main CLI
- `git-remote-nostr` — a git remote helper that enables `nostr://` URLs to work transparently with git

## Detecting a Nostr Repository

Before running any `ngit` commands, check whether the current directory is a nostr repository:

```bash
ngit repo --json
```

Always exits 0. Returns `{"is_nostr_repo": false}` when not in a nostr repo, or the full repo info when it is.

Script usage:

```bash
IS_NOSTR=$(ngit repo --json --offline | jq -r '.is_nostr_repo')
if [ "$IS_NOSTR" = "true" ]; then
  ngit pr list --json
fi
```

---

## Caching and --offline

Every `git fetch` and every `ngit` command that contacts relays fetches and caches the latest PRs, issues, and comments locally. This means **when running multiple ngit commands in quick succession, use `--offline`** on all but the first — it reads from the local cache and is instant.

```bash
# First command fetches from relays and populates the cache
ngit pr list --json

# Subsequent commands in the same session can skip the network
ngit pr view <ID> --offline --comments
ngit issue list --json --offline
ngit repo --json --offline
```

`git fetch origin` also triggers a relay sync, so after a fetch all cached data is fresh.

---

## Core Concepts

### What is a `nostr://` URL?

A `nostr://` URL identifies a repository on the Nostr network. It encodes the maintainer's public key and a short repository identifier:

```
nostr://<npub>/<identifier>
nostr://<npub>/<relay-hint>/<identifier>
```

**`<npub>` is the only reliable form to construct yourself.** It is the bech32-encoded Nostr public key, always starting with `npub1`. Get it from `ngit repo --json` (the `maintainers` field) or `ngit account export-keys`.

The relay hint is a bare domain (no `wss://`) and speeds up discovery — use the repo's grasp server. Get it from `ngit repo --json` (`grasp_servers[0]`).

```
nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit
```

**NIP-05 addresses** (`user@domain.com`) are also valid if the user has set one up, but they require a `/.well-known/nostr.json` file deployed to that domain — do not attempt to construct or guess them. Use the npub form unless a NIP-05 address has been explicitly provided to you.

These URLs work directly with git — no special commands needed:

```bash
git clone nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit
git fetch origin
git push origin main
```

The `git-remote-nostr` helper resolves the URL to actual git servers behind the scenes.

### How repositories work

When a maintainer publishes a repo with `ngit init`, they broadcast a **repository announcement event** to Nostr relays. This event contains:

- Repository name, description, and identifier
- Git server URLs (where the actual git data lives)
- Nostr relay URLs (where events like PRs and issues are published)
- Maintainer public keys

Anyone can clone using the `nostr://` URL. The remote helper fetches the announcement, finds the git servers, and performs the actual git operations.

### Authentication

ngit uses your Nostr private key (nsec) for signing events. Credentials are stored in git config (`nostr.nsec`). Login once globally and all repos use it:

```bash
# Login interactively (stores nsec in global git config)
ngit account login

# Or create a new account
ngit account create --name "Your Name"

# Or pass key inline (for CI/scripts — no prompt)
ngit --nsec <nsec> <command>
```

Credentials are stored as git config keys (`nostr.nsec`, `nostr.npub`, etc.) and can be set directly:

```bash
# Global (all repos)
git config --global nostr.nsec <nsec>

# Local (this repo only)
git config nostr.nsec <nsec>
```

---

## CONCEPT 1: Publishing a Repository

_"Making a git repo discoverable and collaborative on Nostr"_

### First-time publish (from an existing git repo)

```bash
# Interactive (recommended for humans)
ngit init

# Non-interactive / scripted — uses sensible defaults
ngit init --name "My Project" --description "What it does" -d

# With explicit grasp server (hosted git+nostr infrastructure)
ngit init --name "My Project" --description "What it does" \
  --grasp-server https://relay.ngit.dev -d
```

After `ngit init`:

- A repository announcement is published to Nostr relays
- The `origin` remote is updated to the `nostr://` URL
- Your code is pushed to the configured git server(s)
- You get a shareable URL like `https://gitworkshop.dev/npub.../myrepo`

### Update repository metadata

```bash
ngit repo edit --description "Updated description"
ngit repo edit --hashtag rust --hashtag cli
```

### View repository info

```bash
ngit repo
```

---

## CONCEPT 2: Cloning and Working with a Repo

_"Getting and staying in sync with a nostr repository"_

```bash
# Clone using npub + relay hint + identifier (preferred — always works)
git clone nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit

# Clone using npub + identifier (no relay hint — slightly slower discovery)
git clone nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit

# Clone using NIP-05 address if its already been given to you in the clone url.
git clone nostr://dan@danconwaydev.com/ngit

# Standard git operations work normally after cloning
git fetch origin
git pull origin main
git push origin main
```

### Remote branches and PRs

When you fetch from a nostr remote, open and draft PRs appear as remote branches with the `pr/` prefix:

```bash
git fetch origin
git branch -r
# origin/main
# origin/pr/fix-login-bug       ← open PR
# origin/pr/add-dark-mode       ← open PR
```

You can check out a PR branch directly:

```bash
git checkout pr/fix-login-bug
# or by event ID
ngit pr checkout <ID|nevent>
```

---

## CONCEPT 3: Pull Requests

_"Proposing and reviewing changes"_

### Opening a PR

Create a branch with the `pr/` prefix and push it. Always include `-o title=` and `-o description=`:

```bash
git checkout -b pr/my-feature
# ... make commits ...
git push -u origin pr/my-feature \
  -o 'title=Add dark mode' \
  -o 'description=Implements a dark mode toggle in settings.\n\nAdds a toggle to the settings page and persists the preference.'
```

Use `\n\n` in the description for paragraph breaks.

### Opening a PR (advanced — `ngit send`)

```bash
# Send commits ahead of main as a PR
ngit send HEAD~2 --title "My Feature" --description "Details here"

# Non-interactive with defaults
ngit send --defaults

# Update an existing PR (new version/revision)
ngit send HEAD~2 --in-reply-to <PR-event-id>
```

### Listing PRs

```bash
# List open and draft PRs (default)
ngit pr list

# List all statuses
ngit pr list --status open,draft,closed,applied

# Filter by label
ngit pr list --label bug

# Output as JSON (for scripting)
ngit pr list --json
```

### Viewing a PR

```bash
ngit pr view <ID|nevent>

# Include full comment thread
ngit pr view <ID|nevent> --comments

# JSON output
ngit pr view <ID|nevent> --json
```

### Reviewing a PR

```bash
# Checkout the PR branch locally
ngit pr checkout <ID|nevent>

# Or apply patches to current branch
ngit pr apply <ID|nevent>
```

### Commenting on a PR

```bash
ngit pr comment <ID|nevent> --body "Looks good, just one nit..."

# Reply to a specific comment
ngit pr comment <ID|nevent> --body "Fixed!" --reply-to <comment-ID>
```

### Merging a PR (maintainer only)

Prefer `git merge` then push — this creates the merge commit and the push updates the Nostr state automatically:

```bash
# Checkout the PR branch, test it, then merge
ngit pr checkout <ID|nevent>
git checkout main
git merge pr/my-feature
git push origin main
# pushing to the nostr remote records the merge event on Nostr

# Squash merge
git merge --squash pr/my-feature
git commit -m "feat: add dark mode"
git push origin main
```

### PR lifecycle management

```bash
# Close a PR (author or maintainer)
ngit pr close <ID|nevent>

# Reopen a closed PR
ngit pr reopen <ID|nevent>

# Mark a draft PR as ready for review
ngit pr ready <ID|nevent>
```

---

## CONCEPT 4: Issues

_"Tracking bugs, features, and tasks"_

### Creating an issue

```bash
# Interactive
ngit issue create

# Non-interactive
ngit issue create --title "Bug: login fails on mobile" \
  --body "Steps to reproduce: ..." \
  --label bug

# Multiple labels
ngit issue create --title "Add dark mode" \
  --body "Feature request" \
  --label enhancement --label help-wanted
```

### Listing issues

```bash
# List open issues (default)
ngit issue list

# Filter by status
ngit issue list --status open
ngit issue list --status closed

# Filter by label
ngit issue list --label bug

# JSON output
ngit issue list --json
```

### Viewing an issue

```bash
ngit issue view <ID|nevent>

# Include comment thread
ngit issue view <ID|nevent> --comments

# JSON output
ngit issue view <ID|nevent> --json
```

### Commenting on an issue

```bash
ngit issue comment <ID|nevent> --body "I can reproduce this on v1.2.3"

# Reply to a specific comment
ngit issue comment <ID|nevent> --body "Thanks for the report!" --reply-to <comment-ID>
```

### Closing and reopening issues

```bash
# Close (author or maintainer)
ngit issue close <ID|nevent>

# Reopen
ngit issue reopen <ID|nevent>
```

---

## CONCEPT 5: Account Management

_"Managing your Nostr identity"_

All credentials are stored as git config keys. You can set them directly or via `ngit account login`.

```bash
# Login interactively (stores credentials in global git config)
ngit account login

# Login with a bunker:// URL (remote signer / NIP-46)
ngit account login --bunker-url bunker://...

# Login only for this repository (local git config)
ngit account login --local

# Create a new account
ngit account create --name "Alice"

# Export keys (to use in other Nostr clients)
ngit account export-keys

# Logout (removes stored credentials from git config)
ngit account logout
```

Credentials are stored as standard git config entries and can be set or inspected directly:

```bash
# Set nsec globally for all repos
git config --global nostr.nsec <nsec>

# Set nsec for this repo only
git config nostr.nsec <nsec>

# View stored npub
git config --global nostr.npub
```

For CI/automation, pass `--nsec` inline — no login step required:

```bash
ngit --nsec <nsec> issue create --title "CI report" --body "..." -d
```

---

## CONCEPT 6: Sync and Maintenance

_"Keeping git servers in sync with Nostr state"_

```bash
# Sync all refs from nostr state to git servers
ngit sync

# Sync a specific ref
ngit sync --ref-name main
ngit sync --ref-name v1.5.2
```

Use `ngit sync` if git servers have fallen out of sync with the Nostr state (e.g. after relay-side changes).

---

## Common Workflows

### Workflow: Publish a new repository

```bash
cd my-project
git init
git add .
git commit -m "initial commit"
ngit init --name "My Project" --description "A cool project" -d
# origin is now set to nostr://... and code is pushed

# Get the canonical nostr:// URL to share (npub + relay hint + identifier)
ngit repo --json --offline | jq -r '.nostr_url'
# e.g. nostr://npub1abc.../relay.ngit.dev/my-project
```

### Workflow: Contribute a PR to someone else's repo

```bash
git clone nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/relay.ngit.dev/ngit
cd ngit
git checkout -b pr/fix-typo
# ... make changes ...
git commit -m "fix: correct typo in README"
git push -u origin pr/fix-typo \
  -o 'title=Fix typo in README' \
  -o 'description=Corrects a spelling mistake in the introduction.\n\nSmall copy fix, no functional changes.'
```

### Workflow: Review and merge a PR (maintainer)

```bash
# See what's open
ngit pr list

# Review the PR
ngit pr view <ID> --comments

# Check it out and test locally
ngit pr checkout <ID>
# ... run tests ...

# Merge via git and push — push records the merge on Nostr
git checkout main
git merge pr/my-feature
git push origin main
```

### Workflow: File and close an issue

```bash
# File
ngit issue create --title "Crash on startup" \
  --body "Reproducible with v2.1 on Linux.\n\nSteps: run ngit init in an empty dir." \
  --label bug

# Later, close it
ngit issue close <ID>
```

### Workflow: Non-interactive / CI scripting

Use `-d` / `--defaults` to skip all prompts and `--nsec` to provide credentials:

```bash
ngit --nsec <nsec> init --name "My Repo" --description "CI-published repo" -d
ngit --nsec <nsec> issue create --title "Automated report" --body "..." -d
```

---

## Reference: ID formats

Many commands accept an `<ID|nevent>` argument. This can be:

- A hex event ID: `a1b2c3d4...` (64 hex chars)
- A bech32 nevent: `nevent1...`

Get IDs from `ngit pr list --json` or `ngit issue list --json`.

---

## Reference: Push options for PRs

When pushing a `pr/` branch, always set title and description via `-o` push options:

| Option               | Description                                     |
| -------------------- | ----------------------------------------------- |
| `title=<text>`       | PR title                                        |
| `description=<text>` | PR description; use `\n\n` for paragraph breaks |

```bash
git push -u origin pr/my-branch \
  -o 'title=My PR Title' \
  -o 'description=Summary of changes.\n\nMore detail here.'
```

---

## Reference: Key flags

| Flag                  | Description                                                 |
| --------------------- | ----------------------------------------------------------- |
| `-d`, `--defaults`    | Non-interactive mode; use sensible defaults for all prompts |
| `-i`, `--interactive` | Force interactive prompts (default behaviour)               |
| `-f`, `--force`       | Bypass safety guards                                        |
| `-n`, `--nsec <NSEC>` | Provide nsec or hex private key inline                      |
| `--offline`           | Use local cache only, skip network fetch                    |
| `--json`              | Output as JSON (on commands that support it)                |
| `-v`, `--verbose`     | Verbose output                                              |

---

## Reference: git config customisation

All ngit settings live in git config under the `nostr.*` namespace.

```bash
# Show all customisation options
ngit --customize

# Store credentials
git config --global nostr.nsec <nsec>

# Repo-only relay publishing (don't broadcast to personal relays)
git config nostr.repo-relay-only true
```
