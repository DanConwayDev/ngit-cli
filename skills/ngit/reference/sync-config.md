# Sync, flags, and configuration

Part of the ngit skill. Read this when syncing refs, choosing flags, or tuning git config.

## Sync

```bash
ngit sync --json                 # sync all refs from nostr state to git servers
ngit sync --ref-name main --json # sync specific ref
```

## Key flags

| Flag                  | Description                            |
| --------------------- | -------------------------------------- |
| `-d`, `--defaults`    | Non-interactive; use sensible defaults |
| `--offline`           | Local cache only, skip network         |
| `--json`              | Structured output (ngit commands only) |
| `--repo <TARGET>`     | Select remote, naddr, or nostr URL     |
| `--repo-relay-only`   | Publish only to repository relays      |
| `--signer <ALIAS|NPUB|NOSTR-DISPLAY-NAME>` | Use a stored signer for one ngit command |
| `-n`, `--nsec <NSEC>` | Provide nsec or hex private key inline |
| `--nsec-file <PATH>`  | Read a one-shot key from a private file|
| `--nbunksec <NBUNKSEC>` | Provide an established bunker session inline |
| `--nbunksec-file <PATH>` | Read a one-shot bunker session from a private file |
| `-f`, `--force`       | Bypass safety guards                   |
| `-v`, `--verbose`     | Verbose output                         |

## git config

```bash
ngit --customize                          # show all options
git config nostr.repo-relay-only true     # don't broadcast to personal relays
git config nostr.auto-pr-branches false   # fetch PR branches only after checkout
git config nostr.http-io-timeout-ms 600000 # allow large GRASP pushes
git config nostr.secret-storage file      # use ngit's user-only credential file
git config nostr.signer alice             # select the local signer, including for git push
git config nostr.signer-alias.alice npub1... # portable alias-to-npub mapping
git -c nostr.signer=alice push origin pr/topic # select a signer for one Git command
NGIT_CACHE_DIR=/writable/path ngit repo --json # override the global event-cache directory
```

`nostr.auto-pr-branches` defaults to `true` and follows normal Git config
precedence, so repository-local config overrides global config. Add `--global`
to the command above to opt out by default for every repository. When it is
`false`, open and draft PRs are not advertised as `pr/*` branches until
`ngit pr checkout <ID|nevent>` creates the matching local branch. That branch
continues to work with `git fetch` and `git pull`. Run `git fetch --prune` once
to remove PR branches fetched before opting out. To override a global setting
during an initial clone, use
`git clone --config nostr.auto-pr-branches=true <nostr-url>`.

If the global cache directory is unavailable, ngit falls back to an in-memory cache. Repository caches do not: the Git common directory must be writable.
