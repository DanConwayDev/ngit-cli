# Accounts — identity, login, secrets

Part of the ngit skill. Read this when managing accounts, logins, or credential storage.

## Commands

```bash
ngit account whoami --json
ngit account whoami --json --offline          # list from config/cache only
ngit account list --json --offline            # alias for account whoami
ngit account login                            # interactive; stores the secret in a credential store
ngit account login alice                      # make a retained account the global default
ngit account login --local alice              # make a retained account this repo's default
ngit account login alice --alias work         # add an alias and make alice the global default
ngit account login --bunker-url bunker://...  # NIP-46 remote signer
ngit account login --local                    # this repo only
ngit account login --secret-storage file      # bypass the OS store; use ngit's user-only file store
ngit account login --secret-storage git-config # explicitly allow plaintext git-config storage
ngit account login --nsec-file /private/key --alias alice # store a reusable alias
ngit account login --nbunksec-file /private/connection --alias alice # store an established bunker session
ngit account login --local --alias alice       # activate a retained alias for this repository
ngit account login --local -i --alias alice    # pair a fresh signer and assign or replace this alias
ngit account create --name "Alice" --json
ngit account export-keys --json                # returns nsec or nbunksec for the selected account
ngit account export-keys --secret              # print only the selected nsec or nbunksec
ngit account logout --json                    # removes login config, but preserves stored keys
ngit account logout --forget --json           # logout and delete the stored secret
ngit account forget-keys <entry> --json       # delete a preserved credential-store entry
ngit --signer alice issue create --subject "Bug" --body "Details" --json # sign one ngit command as alice
git -c nostr.signer=alice push origin pr/topic # run one Git command as alice
ngit --nsec <nsec> <command>                  # inline for CI, no login needed
ngit --nsec-file /private/key <command>       # one-shot CI/agent key, omitted from argv
ngit --nbunksec <nbunksec> <command>           # inline established bunker session
ngit --nbunksec-file /private/connection <command> # one-shot bunker session, omitted from argv
```

`account whoami` combines usable signers from the current repository's local Git
config, global/system Git config, the OS credential store, and
`credentials.json`. It groups identities by npub and lists every distinct
local-key or remote-signer connection with the npub and aliases that select it.
Scope and active badges appear on the actual connection, so different local
and global NIP-46 sessions for one identity remain distinguishable. It prints
ready-to-use `ngit --signer ... <command>` and
`git -c nostr.signer=... <command>` guidance once beneath the inventory.
The same footer explains global and repository login, alias creation, and how
logout reveals a shadowed global default. `ACCOUNT` can be a full npub, a
listed alias, or the exact cached Nostr profile name. `account list` is an
alias for this same command and JSON shape.

By default, login/create use the OS credential store and fall back to ngit's user-only file store. Git config contains the credential entry name rather than the secret. Select `auto`, `file`, or `git-config` with `--secret-storage`, `NGIT_SECRET_STORAGE`, or `nostr.secret-storage`; plaintext git-config storage must be requested explicitly. Existing plaintext values remain supported.

Aliases name stored signers without exposing their secrets. On ordinary
commands, `--signer <alias|npub|nostr-display-name>` selects an identity for one
direct `ngit` invocation without rewriting the configured login. For a Git
command such as `push`, use
`git -c nostr.signer=<alias|npub|nostr-display-name> <command>` for the same
one-shot behavior.
`ngit account login <account>` deliberately activates that stored signer; add
`--local` to make it the repository default, including for `git push`, or omit
`--local` to make it the global default. The older `ngit account login
--signer <account>` spelling remains available.
`ngit account login --local --alias <alias>` provides the same reactivation
shorthand. Add `-i` to pair a fresh signer under that alias instead; if the
repository already uses it, choose the logout option and the new connection
will replace the alias without replacing the identity's default connection.
A profile name (for example `--signer "DanConwayDev's Agent"`)
resolves against cached kind-0 profiles of accounts that hold stored
credentials; it must match exactly one such account, and only the resolved
npub is ever persisted. Explicit signer selection fails closed when the
selector is missing, ambiguous, or backed by invalid credentials.

An npub may have more than one NIP-46 connection. The first connection remains
the default selected by the bare npub; ngit refuses to replace it with a
different unaliased connection. Add `--alias <name>` while logging in to retain
an additional connection, then use that alias with `--signer`,
`git -c nostr.signer=...`, or `account export-keys` to select that exact
connection. A collision does not offer plaintext Git config as a fallback;
that storage mode remains available only when explicitly selected with
`--secret-storage git-config`.
Credential-store alias values remain raw npubs for compatibility with older
ngit versions. Exact remote-session selection uses a separate public binding;
older versions ignore it and select the identity's default signer.
If no default remains, a sole alias-specific connection can also be selected
by npub; multiple remaining connections require an alias.

For remote signers, `nbunksec` is a portable established connection containing
the remote-signer pubkey, client/app secret key, relays, and optional original
pairing secret. It does not contain the user's npub, so one-shot use resolves
the identity with `get_public_key`. A stored login associates that result with
the connection. Prefer `--nbunksec-file` in CI so the credential does not
appear in process arguments.
