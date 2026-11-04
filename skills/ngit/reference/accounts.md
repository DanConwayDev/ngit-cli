# Accounts — identity, login, secrets

Read when managing accounts, logins, or credential storage. Guide:
https://ngit.dev/accounts (storage modes, pairing a remote signer for CI,
rotation).

```bash
ngit account whoami --json --offline            # every usable signer with npub, aliases, scope, active state; `account list` is an alias
ngit account login -i                           # interactive; the secret goes to the OS credential store or ngit's user-only file store
ngit account login alice                        # make a stored identity the global default (alias, npub, or exact profile name)
ngit account login --local alice                # …this repository's default, including for git push
ngit account login --nsec-file /private/key --profile alice
ngit account login --nbunksec-file /private/connection --profile alice   # store an established NIP-46 session
ngit account login --bunker-url bunker://...    # pair a remote signer
ngit account login --local -i --profile alice     # pair a fresh signer and assign or replace this profile
ngit account login -i --secret-storage file        # bypass the OS store; `git-config` stores plaintext and must be explicit
ngit account create --name "Alice" --json
ngit account export-keys --secret               # print only the nsec or nbunksec; --json adds the npub
ngit account logout --json                      # forget selected account; report remaining logins
ngit account logout --all                       # remove known remote sessions and clear current/non-local config
ngit account logout --all --forget              # also delete identity nsecs (explicitly destructive)
ngit account logout --keep-credentials          # retain remote session for reactivation
ngit account logout --profile alice --forget    # delete stored profile even after logout
ngit account forget-keys <npub> --json          # delete all stored credentials for this identity, including its key
ngit --signer alice issue create --subject "Bug" --body "Details" --json   # one ngit command as alice
git -c nostr.signer=alice push origin pr/topic  # one git command as alice
ngit --nsec-file /private/key <command>         # one-shot key for CI; --nbunksec-file for a bunker session
```

**Storage.** `auto` (OS store, then the file store), `file`, or `git-config`,
selected with `--secret-storage`, `NGIT_SECRET_STORAGE`, or
`nostr.secret-storage`. Except in explicit `git-config` mode, Git config holds a public selection or
credential pointer, not the secret. Existing plaintext values keep working.

**Selection.** `--signer` and `nostr.signer` accept an alias, an npub, or an
exact cached profile name. A profile name must match exactly one account that
holds stored credentials, and only the resolved npub is persisted. Selection
fails closed when the selector is missing, ambiguous, or backed by invalid
credentials.

**Several NIP-46 connections for one npub.** Fresh login replaces the default
remote session for that npub. Use `--profile NAME` to keep another connection
or replace that named profile's connection. Other named profiles retain their
exact sessions. An existing nsec is preserved; use a named profile to add a
remote signer beside it.

**nbunksec** is a portable established connection: remote-signer pubkey, client
secret, relays, and optional pairing secret. It holds no npub, so one-shot use
resolves the identity from the signer. The `--nbunksec-file` and `--nsec-file`
forms keep secrets out of process arguments. A fresh pairing needs interactive
approval, so unattended runs use a stored connection.

**Logout scope and deletion.** Plain logout forgets the repository-specific
account when one is selected, otherwise the default account. Removing the
repository override activates a remaining default account; read the reported
account before signing again. A second plain logout then targets that default.
Use `--local` to restrict logout to the repository override, or `--global` to
target the default. Credentials shared with other repositories are removed too;
different named profiles remain independent even for the same npub.

Deletion of an identity nsec returns an error before changes, with no prompt.
Use `--forget` to authorize deletion or `--keep-credentials` to retain credentials.
Remote session credentials (including nbunksec) do not need `--forget`.
Targeted logout works after deselection and without contacting the remote signer.

`logout --all` removes all known profiles from the OS and file stores and clears
current-repository and non-local Git login configuration, including referenced
includes. It does not search other repositories for independent credentials or
revoke remote signer permissions. Store/config cleanup failures are errors.
The JSON result lists remaining accounts and unavailable login selections.

**Explicit npub removal.** `account logout <npub>` targets all known credentials
and profile bindings for that identity, not just its default connection; add
`--forget` when identity keys are included. `account forget-keys <npub>` is the
explicit destructive operation and includes identity keys without another
confirmation. Use `logout --profile NAME` for one profile, or `forget-keys
signer:<npub>` for only the default remote session. Other identities are untouched.
