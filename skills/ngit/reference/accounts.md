# Accounts — identity, login, secrets

Part of the ngit skill. Read this when managing accounts, logins, or credential storage.

## Commands

```bash
ngit account whoami --json
ngit account whoami --json --offline          # use cache, no network
ngit account login                            # interactive; stores the secret in a credential store
ngit account login --bunker-url bunker://...  # NIP-46 remote signer
ngit account login --local                    # this repo only
ngit account login --secret-storage file      # bypass the OS store; use ngit's user-only file store
ngit account login --secret-storage git-config # explicitly allow plaintext git-config storage
ngit account create --name "Alice"
ngit account export-keys
ngit account logout                           # removes login config, but preserves stored keys
ngit account logout --forget                  # logout and delete the stored secret
ngit account forget-keys <entry>              # delete a preserved credential-store entry
ngit --nsec <nsec> <command>                  # inline for CI, no login needed
ngit --nsec-file /private/key <command>       # one-shot CI/agent key, omitted from argv
```

By default, login/create use the OS credential store and fall back to ngit's user-only file store. Git config contains the credential entry name rather than the secret. Select `auto`, `file`, or `git-config` with `--secret-storage`, `NGIT_SECRET_STORAGE`, or `nostr.secret-storage`; plaintext git-config storage must be requested explicitly. Existing plaintext values remain supported.