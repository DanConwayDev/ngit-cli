# Credential storage

How ngit stores nostr secrets, and the git-config convention other nostr
applications can rely on.

## Where secrets live

`ngit account login` stores the secret in the OS credential store (macOS
Keychain, Windows Credential Manager, or the D-Bus secret service on Linux)
via the [`keyring`] crate, under keyring **service `ngit`**. When no OS
credential store is available, the secret goes to ngit's **file store**
instead: a JSON file at `<ngit-data-dir>/credentials.json` (on Linux
`~/.local/share/ngit/credentials.json`) restricted to the current user
(0700 directory, 0600 file on unix).

The file store is plaintext by design. The threat this feature counters is
*incidental* disclosure of git config — which coding agents and other tools
read routinely — not filesystem compromise by a targeted attacker. A secret
in an ngit-specific path is far less likely to be casually read and
republished than one sitting in `~/.gitconfig`.

Entries are named by the npub of the stored key itself, so every login for
the same account shares one entry and the name remains derivable after
logout. Bunker (NIP-46) logins store the app key under the app key's own
npub. Entries written by pre-release versions as `<npub>/<8-char-suffix>`
are still read.

Git config remains the index, because platform keyrings cannot be
enumerated:

| git config key         | value                                                        |
| ---------------------- | ------------------------------------------------------------ |
| `nostr.nsec`           | credential entry name, plaintext `nsec1…`, or `ncryptsec1…`  |
| `nostr.bunker-app-key` | credential entry name or plaintext key                       |

## Choosing where secrets live

The `nostr.secret-storage` git config item — overridden by the
`NGIT_SECRET_STORAGE` environment variable, or per invocation by
`ngit account login --secret-storage <value>` — selects the policy:

- `auto` (default): OS credential store, falling back to the file store.
- `file`: ngit's file store only, never the OS store.
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
account/user field) of an entry under keyring service `ngit`. The npub is
derived from the stored secret itself and must be verified against the
retrieved key on read.

The entry's secret is the **32 raw bytes** of the secret key, with no
encoding or envelope — the representation `nostr-keyring` used, so
applications built on that crate interoperate without changes.

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

[`keyring`]: https://crates.io/crates/keyring
