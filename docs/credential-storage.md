# Credential storage

How ngit stores nostr secrets, and the git-config convention other nostr
applications can rely on.

## Where secrets live

`ngit account login` stores the secret in the OS credential store (macOS
Keychain, Windows Credential Manager, or the D-Bus secret service on Linux)
via the [`keyring`] crate, under keyring **service `nostr`**. When no OS
credential store is available, the secret goes to ngit's **file store**
instead: a JSON file at `<ngit-data-dir>/credentials.json` (on Linux
`~/.local/share/ngit/credentials.json`) restricted to the current user
(0700 directory, 0600 file on unix). Updates write and flush a complete
replacement beside the file before atomically replacing it on Unix and
Windows, so interruption cannot leave a partially written credential file.

The file store is plaintext by design. The threat this feature counters is
*incidental* disclosure of git config — which coding agents and other tools
read routinely — not filesystem compromise by a targeted attacker. A secret
in an ngit-specific path is far less likely to be casually read and
republished than one sitting in `~/.gitconfig`.

Local-key entries are named by the npub of the stored key itself, so every login for
the same account shares one entry and the name remains derivable after
logout. Bunker (NIP-46) logins use `signer:<user-npub>` and contain one typed,
versioned record with the expected user npub, sanitized bunker URI, and bunker
client/app nsec. The one-time `secret=` pairing parameter is removed before
the record is saved. Entries written by pre-release versions as
`<npub>/<8-char-suffix>` are still read.

Git config remains the index, because platform keyrings cannot be
enumerated:

| git config key         | value                                                        |
| ---------------------- | ------------------------------------------------------------ |
| `nostr.nsec`           | credential entry name, plaintext `nsec1…`, or `ncryptsec1…`  |
| `nostr.bunker-app-key` | credential entry name or plaintext key                       |

## Selecting signers

`--signer <npub|alias|profile-name>` selects an existing signer for one
command without changing the configured profile. An alias maps to an npub in
any of these places:

- OS credential store: `alias:fred` contains `npub1…`
- `credentials.json`: `nostr/alias:fred` contains `npub1…`
- git config: `nostr.signer-alias.fred = npub1…`

Aliases are resolved from the OS credential store first, then
`credentials.json`, then local, global, and system Git config. The JSON store
is the direct fallback for systems where the OS credential store is
unavailable. `nostr.signer = fred` (or an
npub) makes a signer the default for that Git-config scope. `ngit account login
--alias fred` writes the mapping to the selected Git-config scope and, unless
`git-config` secret storage was selected, the selected credential backend as
well. After logout retains a stored signer, `ngit account login --local --alias
fred` reactivates it without requiring the nsec or bunker URL again.

A cached profile name is a third selector form: `--signer DanConwayDev` or
`--signer "DanConwayDev's Agent"`. Selector precedence is npub, then alias,
then profile name — a name is only consulted when the selector is not an
npub and no alias mapping exists (including selectors that could never be
alias tokens). The name is compared case-insensitively, after trimming,
against the `name` and `display_name` fields of the newest cached kind-0
profile per account, and only accounts with stored signer credentials count,
so a same-named profile cached from someone else's account cannot be
selected. Exactly one credentialed account may match: no match produces
guidance to select by npub or alias instead (profiles enter the cache when
their account logs in), and two or more matches fail closed listing each
candidate's name and npub. A broken or unavailable credential entry for a
matching account fails the selection rather than being skipped. Because
profile names are mutable and non-unique they are never persisted:
`ngit account login --signer <name>` resolves the name once and writes the
resolved npub to `nostr.signer`, exactly like npub reactivation, while a
one-shot `--signer <name>` re-resolves on every invocation and writes
nothing.

For a credential-store-backed selection, the selected Git-config scope contains
`nostr.signer` and `nostr.npub`, plus `nostr.signer-alias.<alias>` when an alias
is used; it does not keep a redundant `nostr.nsec` or bunker pointer. With
`secret-storage = git-config`, the nsec or bunker fields remain in that scope
because they are the signer material rather than credential-store pointers.

An alias stored in the OS credential store or `credentials.json` is
machine-wide and cannot be reassigned to another npub by logging in again;
choose a new alias or explicitly remove `alias:<name>` first. Git-config-only
aliases can differ between repositories when `secret-storage = git-config`,
provided no higher-priority credential-store alias uses the name, and follow
local, global, then system scope precedence.

After resolving an alias to its npub, ngit checks all nsec sources before any
bunker source. Within each type the order is OS credential store,
`credentials.json`, then matching local/global/system Git config. Bunker
fields are never assembled across scopes. The chosen nsec is checked by
deriving its public key. A bunker's user public key is obtained once during
the initial NIP-46 pairing and persisted with its connection details. Later
commands seed that stored key into the connection instead of making a new
identity request or signing a verification-only event. Every real event
returned by the bunker must still match the requested public key and event ID
and carry a valid signature. Missing, malformed, or mismatched explicit
selections fail instead of falling back to a different identity.

When `--signer` is omitted, existing flat local/global/system login selection
continues to work. A configured `nostr.signer` opts that scope into the new
selection model.

## Choosing where secrets live

The `nostr.secret-storage` git config item — overridden by the
`NGIT_SECRET_STORAGE` environment variable, or per invocation by
`ngit account login --secret-storage <value>` — selects the policy:

- `auto` (default): OS credential store, falling back to the file store.
- `file`: write to ngit's file store, never the OS store. If different data
  already exists under the same higher-priority OS entry, login fails with a
  removal command instead of writing a credential that could never be used.
- `git-config`: plaintext in git config, as ngit stored secrets previously.

When a secret cannot be stored under `auto` or `file`, login fails with
guidance (interactively, it offers the plaintext fallback explicitly)
instead of silently writing plaintext to git config.

The pre-release `nostr.credential-store` / `NGIT_CREDENTIAL_STORE` boolean
is still read: `false` maps to `git-config`, `true` to `auto`.

## Logout

`ngit account logout` removes the login from git config but deliberately
keeps the stored secret: the credential store may hold the only copy of an
identity key, so deleting it on logout could destroy the account. Logout
prints the exact command to remove the secret as well —
`ngit account forget-keys <entry>` — and `ngit account logout --forget`
does both in one step.

## Interop convention

A value of `nostr.nsec` / `nostr.bunker-app-key` that is a bare `npub1…`
(or the legacy `npub1…/<8 alphanumeric chars>` form) is the name (the
account/user field) of an entry under keyring service `nostr`. The npub is
derived from the stored secret itself and must be verified against the
retrieved key on read.

The service is `nostr`, not `ngit`, because these entries pair with `nostr.*`
git config keys and are useful to other nostr applications. Namespaces keep
record types distinct: bare `npub1…` for an identity nsec, `signer:npub1…` for
a typed bunker record, and `alias:<name>` for an alias mapping.

The entry's secret is written as an **`nsec1…` bech32 string**, so the
platform's own credential UI can display it and a user can recover the key
without ngit. Readers should also accept a **64-character hex string** for
compatibility with applications that use that textual representation. Other
values are corrupt rather than alternate encodings to guess at.

Plaintext and `ncryptsec1…` values remain valid indefinitely;
applications without credential-store support can keep writing plaintext.

## Plaintext values

ngit reads plaintext `nsec1…` / app-key values from git config indefinitely
and never rewrites them: a read path that migrates credential storage nags
on every command when no store is available, and inside a sandboxed or
ephemeral environment it could strand the only copy of a key in a store
that is about to disappear.

To move an existing plaintext login into a credential store, log in again
with `ngit account login`. Interactive commands print a once-per-run hint
to that effect while a plaintext secret is in use.

## One-shot non-interactive use

`--nsec-file PATH` reads one nsec or hex key for the current command without
placing it in argv or git config. The path may contain or be a symlink, but its
opened target must be a regular file with one non-empty line and at most 4096
bytes. On Unix, the target's mode must be 0400 or 0600. On Windows, filesystem
ACLs control access and ngit does not audit the target's ACL. It conflicts with
`--nsec`. Prefer `ngit account login` for reusable identities.

[`keyring`]: https://crates.io/crates/keyring
