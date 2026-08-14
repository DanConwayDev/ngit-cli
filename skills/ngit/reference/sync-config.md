# Sync, flags, and configuration

Part of the ngit skill. Read this when syncing refs, choosing flags, or tuning git config.

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
| `--repo <TARGET>`     | Select remote, naddr, or nostr URL     |
| `--repo-relay-only`   | Publish only to repository relays      |
| `--signer <NPUB|ALIAS|NAME>` | Use a stored signer for one command |
| `-n`, `--nsec <NSEC>` | Provide nsec or hex private key inline |
| `--nsec-file <PATH>`  | Read a one-shot key from a private file|
| `-f`, `--force`       | Bypass safety guards                   |
| `-v`, `--verbose`     | Verbose output                         |

## git config

```bash
ngit --customize                          # show all options
git config nostr.repo-relay-only true     # don't broadcast to personal relays
git config nostr.http-io-timeout-ms 600000 # allow large GRASP pushes
git config nostr.secret-storage file      # use ngit's user-only credential file
git config nostr.signer alice             # select the local signer, including for git push
git config nostr.signer-alias.alice npub1... # portable alias-to-npub mapping
NGIT_CACHE_DIR=/writable/path ngit repo --json # override the global event-cache directory
```

If the global cache directory is unavailable, ngit falls back to an in-memory cache. Repository caches do not: the Git common directory must be writable.
