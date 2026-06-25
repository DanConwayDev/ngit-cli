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

- **`pr/` prefix is MANDATORY for PRs** — branch names for pull requests MUST start with `pr/` (e.g. `pr/my-feature`). A branch without this prefix is a plain git push and will never create a PR.
- **Always use `--json`** on `ngit` commands when reading output — far easier to parse than human-readable text. `git` commands do not support `--json`.
- **Use `--offline`** on all but the first `ngit` command in a session — reads from local cache instantly. `git fetch origin` also refreshes the cache.
- **Never construct NIP-05 addresses** (`user@domain`). Use the `npub1...` form unless a NIP-05 address was explicitly provided.
- **`<ID|nevent>`** accepts a 64-char hex event ID or a `nevent1...` bech32 string. Get IDs from `ngit pr list --json` or `ngit issue list --json`.
- **`--json` output uses `nevent1…` bech32** for all `id` and `reply_to` fields (not raw hex). Use these values directly as `<ID|nevent>` arguments and in `nostr:` URI references.
- **Reference other issues/PRs/comments in `--body` using `nostr:` URIs** — e.g. `nostr:nevent1abc…` or `nostr:naddr1abc…`. Never paste raw hex IDs into body text. The `id` field from `--json` output is already a valid `nevent1…` string; prefix it with `nostr:` to form the URI. Example: `--body "Relates to nostr:nevent1abc…"`. ngit automatically converts these into the correct event tags.

## Detecting a nostr repo

```bash
git remote -v | grep -q 'nostr://'   # primary check — no cache needed
ngit repo --json --offline            # full metadata when needed
```

`ngit repo` always exits 0; `is_nostr_repo: false` can be a cold-cache false negative — if remotes show `nostr://`, run `git fetch origin` then retry. Full output includes `nostr_url`, `maintainers`, `grasp_servers`.

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

> **CRITICAL: Branch name MUST start with `pr/`** — this is what signals ngit to create a PR. A branch without the `pr/` prefix is a plain push and will NEVER create a PR, regardless of push options.

```bash
git checkout -b pr/my-feature          # MUST use pr/ prefix — not "my-feature", not "feature/foo"
# ... commits ...

# Single commit: omit title/description — commit subject and body are used automatically (preferred)
git push -u origin pr/my-feature

# Multiple commits: supply title and description explicitly
# Use literal \n\n for paragraph breaks — ngit's push-option parser converts them to real newlines.
# Do NOT use $'...\n\n...' ANSI-C quoting — git cannot pass real newlines through push options.
git push -u origin pr/my-feature \
  -o 'title=My feature title' \
  -o 'description=First paragraph.\n\nSecond paragraph.'
```

When there is only one commit, omitting `-o title=` and `-o description=` is preferred — ngit uses the commit subject as the title and the commit body as the description. Pass `-d` (or `--defaults`) to confirm this automatically. `git push` or `git push --force` can update existing PRs (branch must still have the `pr/` prefix).

### Advanced: ngit send

`ngit send` takes `--description` as a regular shell argument — the shell does **not** interpret `\n` inside double-quoted strings, so `"...\n\n..."` produces literal backslash-n in the event. Use ANSI-C quoting (`$'...'`) to embed real newlines:

```bash
# correct — $'...' quoting gives real newlines
ngit send HEAD~2 \
  --subject "My Feature" \
  --description $'First paragraph.\n\nSecond paragraph.'

# WRONG — \n inside double quotes is not interpreted; event contains literal \n\n
ngit send HEAD~2 --subject "My Feature" --description "First paragraph.\n\nSecond paragraph."

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
ngit merge <ID|nevent>                    # merge PR into default branch; does not push
ngit pr checkout <ID|nevent>
ngit merge                                # infers PR from checked-out pr/ branch
ngit merge --exclude-description <ID|nevent>
git push origin main                      # publishes the merge event
```

`ngit merge` creates a no-ff merge commit on the default branch with the
standard `Merge #<8-hex>: <PR title>` message. If conflicts occur, resolve them
and run `git commit`; ngit has already prepared the commit message.

### Lifecycle

```bash
ngit pr close <ID|nevent> --reason "blocked by upstream"
ngit pr reopen <ID|nevent> --reason "fix was incomplete"
ngit pr ready <ID|nevent> --reason "addressed review feedback"
ngit pr draft <ID|nevent> --reason "needs more work"
ngit pr label <ID|nevent> --label bug --label enhancement
ngit pr set-subject <ID|nevent> --subject "New title"
ngit pr set-cover-note <ID|nevent> --body "Updated description. See nostr:nevent1abc…"
```

## Issues

```bash
ngit issue create --subject "Bug title" --body "Details as markdown" --label bug
ngit issue create --subject "Feature" --body "..." --label enhancement --label help-wanted
ngit issue list --json
ngit issue list --json --status closed
ngit issue list --json --label bug
ngit issue view <ID|nevent> --json
ngit issue view <ID|nevent> --json --comments
ngit issue comment <ID|nevent> --body "Reproduced on v2.1"
ngit issue comment <ID|nevent> --body "Thanks!" --reply-to <comment-ID|nevent>
ngit issue close <ID|nevent> --reason "wontfix"
ngit issue resolved <ID|nevent> --reason "fixed in abc123"
ngit issue reopen <ID|nevent> --reason "regression in v2.3"
ngit issue label <ID|nevent> --label bug --label enhancement
ngit issue set-subject <ID|nevent> --subject "New title"
ngit issue set-cover-note <ID|nevent> --body "Updated description. See nostr:nevent1abc…"
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
git config nostr.http-io-timeout-ms 600000 # allow large GRASP pushes
```
