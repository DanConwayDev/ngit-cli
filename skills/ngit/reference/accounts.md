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
ngit account login --nsec-file /private/key --alias alice # store a reusable alias
ngit account login --local --alias alice       # activate a retained alias for this repository
ngit account create --name "Alice" --json
ngit account export-keys --json
ngit account logout --json                    # removes login config, but preserves stored keys
ngit account logout --forget --json           # logout and delete the stored secret
ngit account forget-keys <entry> --json       # delete a preserved credential-store entry
ngit --signer alice account whoami --json --offline # inspect a stored identity without switching
ngit --signer alice issue create --subject "Bug" --body "Details" --json # sign one ngit command as alice
git -c nostr.signer=alice push origin pr/topic # run one Git command as alice
ngit --nsec <nsec> <command>                  # inline for CI, no login needed
ngit --nsec-file /private/key <command>       # one-shot CI/agent key, omitted from argv
```

By default, login/create use the OS credential store and fall back to ngit's user-only file store. Git config contains the credential entry name rather than the secret. Select `auto`, `file`, or `git-config` with `--secret-storage`, `NGIT_SECRET_STORAGE`, or `nostr.secret-storage`; plaintext git-config storage must be requested explicitly. Existing plaintext values remain supported.

Aliases name stored signers without exposing their secrets. On ordinary
commands, `--signer <alias|npub|nostr-display-name>` selects an identity for one
direct `ngit` invocation without rewriting the configured login. For a Git
command such as `push`, use
`git -c nostr.signer=<alias|npub|nostr-display-name> <command>` for the same
one-shot behavior.
`ngit account login --signer <alias>` deliberately activates that stored
signer; add `--local` to make it the repository default, including for
`git push`, or omit `--local` to make it the global default.
`ngit account login --local --alias <alias>` provides the same reactivation
shorthand. A profile name (for example `--signer "DanConwayDev's Agent"`)
resolves against cached kind-0 profiles of accounts that hold stored
credentials; it must match exactly one such account, and only the resolved
npub is ever persisted. Explicit signer selection fails closed when the
selector is missing, ambiguous, or backed by invalid credentials.
