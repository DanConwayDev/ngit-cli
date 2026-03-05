---
name: ngit
description: Provides commands and workflows for nostr:// git repositories using the ngit CLI and git-remote-nostr. Activates when working with nostr:// remotes or URLs, ngit commands, gitworkshop.dev repositories, submitting or reviewing pull requests (PRs) or patches over Nostr, creating or viewing Nostr git issues, cloning a nostr:// URL, publishing a repository with ngit init, or any task involving the ngit CLI or git-remote-nostr.
license: CC-BY-SA-4.0
metadata:
  version: "1.0"
---

# ngit — Nostr Plugin for Git

ngit makes `clone`, `fetch`, `push` work with `nostr://` URLs and adds a CLI for PRs, issues, and repo management over the decentralised Nostr protocol.

- Install: `curl -Ls https://ngit.dev/install.sh | bash` (installs `ngit` and `git-remote-nostr`)
- Web UI: https://gitworkshop.dev

## How it works

**Nostr** is a decentralised protocol where users publish signed events to relays (simple servers anyone can run). There is no central authority — identity is a keypair, and data is replicated across many relays.

Git has two distinct layers that ngit separates:

- **Git state (refs)** — which commit each branch/tag points to — is published as signed events on Nostr relays. This is the source of truth for the repository.
- **Git data (objects)** — the actual commits, trees, and blobs — is stored on ordinary git servers (any server that speaks the git protocol).

When you `git fetch`, `git-remote-nostr` reads the current ref state from Nostr relays, then fetches the corresponding objects from the git server(s) listed in the repository announcement. Because the state lives on Nostr and the data can live anywhere, git servers are interchangeable — switching providers requires no coordination with contributors.

**Grasp servers** are a convenience: they combine a Nostr relay and a git server into a single hosted service (e.g. `relay.ngit.dev`). When `ngit init` publishes a repository announcement listing a grasp server, the grasp server automatically creates the git repository — no prior setup or account configuration required. You can use separate relays and git servers if you prefer.

## Key rules

- **Always use `--json`** on `ngit` commands when reading output — far easier to parse than human-readable text. `git` commands do not support `--json`.
- **Use `--offline`** on all but the first `ngit` command in a session — reads from local cache instantly. `git fetch origin` also refreshes the cache.
- **Never construct NIP-05 addresses** (`user@domain`). Use the `npub1...` form unless a NIP-05 address was explicitly provided.
- **`<ID|nevent>`** accepts a 64-char hex event ID or a `nevent1...` bech32 string. Get IDs from `ngit pr list --json` or `ngit issue list --json`.

## Detecting a nostr repo

```bash
# Check if current directory is a nostr repo (always exits 0)
ngit repo --json --offline | grep -q '"is_nostr_repo":true'
```

Returns full repo info (including `nostr_url`, `maintainers`, `grasp_servers`) when true.

## nostr:// URLs

```
nostr://<npub>/<identifier>
nostr://<npub>/<relay-hint>/<identifier>   # relay-hint is bare domain, e.g. relay.ngit.dev
```

Standard git commands work directly with these URLs — `git-remote-nostr` resolves them transparently.

## Publishing a repo

```bash
ngit init --name "My Project" --description "What it does" -d # uses user's preferred grasp server or falls back to defaults
ngit repo edit --description "New description"                   # update metadata
ngit repo --json --offline                                       # view repo info (check nostr_url field)
```

## Cloning

```bash
git clone nostr://<npub>/<relay-hint>/<identifier>   # preferred
git clone nostr://<npub>/<identifier>                # slower discovery, no relay hint
git clone nostr://user@domain.com/<identifier>       # NIP-05, only if given to you
```

## Pull Requests

### Open a PR

```bash
git checkout -b pr/my-feature
# ... commits ...
git push -u origin pr/my-feature \
  -o 'title=My feature title' \
  -o 'description=Summary.\n\nDetail here.'
```

Push options `title=` and `description=` are required. Use `\n\n` for paragraph breaks. `git push` or `git push --force` can update existing prs.

### Advanced: ngit send

```bash
ngit send HEAD~2 --title "My Feature" --description "Details"
ngit send --defaults                                    # non-interactive
ngit send HEAD~2 --in-reply-to <PR-event-id>           # update existing PR
```

### List / view / comment

```bash
ngit pr list --json
ngit pr list --json --status open,draft,closed,applied
ngit pr list --json --label bug
ngit pr view <ID|nevent> --json
ngit pr view <ID|nevent> --json --comments
ngit pr comment <ID|nevent> --body "Looks good"
ngit pr comment <ID|nevent> --body "Fixed!" --reply-to <comment-ID|nevent>
```

### Checkout / apply

```bash
ngit pr checkout <ID|nevent>
```

### Merge (maintainer)

```bash
ngit pr checkout <ID|nevent>
git checkout main
git merge pr/my-feature   # or: git merge --squash pr/my-feature && git commit
git push origin main      # push to nostr remote records the merge event
```

### Lifecycle

```bash
ngit pr close <ID|nevent>
ngit pr reopen <ID|nevent>
ngit pr ready <ID|nevent>   # mark draft as ready for review
ngit pr label <ID|nevent> --label bug --label enhancement
```

## Issues

```bash
ngit issue create --title "Bug title" --body "Details as markdown" --label bug
ngit issue create --title "Feature" --body "..." --label enhancement --label help-wanted
ngit issue list --json
ngit issue list --json --status closed
ngit issue list --json --label bug
ngit issue view <ID|nevent> --json
ngit issue view <ID|nevent> --json --comments
ngit issue comment <ID|nevent> --body "Reproduced on v2.1"
ngit issue comment <ID|nevent> --body "Thanks!" --reply-to <comment-ID|nevent>
ngit issue close <ID|nevent>
ngit issue reopen <ID|nevent>
ngit issue label <ID|nevent> --label bug --label enhancement
```

## Account management

```bash
ngit account whoami --json
ngit account whoami --json --offline          # use cache, no network
ngit account login                            # interactive, stores nsec in global git config
ngit account login --bunker-url bunker://...  # NIP-46 remote signer
ngit account login --local                    # this repo only
ngit account create --name "Alice"
ngit account export-keys
ngit account logout
git config --global nostr.nsec <nsec>         # set directly
ngit --nsec <nsec> <command>                  # inline for CI, no login needed
```

## Sync

```bash
ngit sync                        # sync all refs from nostr state to git servers
ngit sync --ref-name main        # sync specific ref
```

## Key flags

| Flag                  | Description                            |
| --------------------- | -------------------------------------- |
| `-d`, `--defaults`    | Non-interactive; use sensible defaults |
| `--offline`           | Local cache only, skip network         |
| `--json`              | Structured output (ngit commands only) |
| `-n`, `--nsec <NSEC>` | Provide nsec or hex private key inline |
| `-f`, `--force`       | Bypass safety guards                   |
| `-v`, `--verbose`     | Verbose output                         |

## git config

```bash
ngit --customize                          # show all options
git config nostr.repo-relay-only true     # don't broadcast to personal relays
```
