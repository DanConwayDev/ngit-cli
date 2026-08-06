# Credential storage

How ngit stores nostr secrets, and the git-config convention other nostr
applications can rely on.

## Where secrets live

`ngit account login` stores the secret in the OS credential store (macOS
Keychain, Windows Credential Manager, or the D-Bus secret service on Linux)
via [`nostr-keyring`], under keyring **service `ngit`**. When no OS
credential store is available, the secret goes to ngit's **file store**
instead: a JSON file at `<ngit-data-dir>/credentials.json` (on Linux
`~/.local/share/ngit/credentials.json`) restricted to the current user
(0700 directory, 0600 file on unix).

The file store is plaintext by design. The threat this feature counters is
*incidental* disclosure of git config — which coding agents and other tools
read routinely — not filesystem compromise by a targeted attacker. A secret
in an ngit-specific path is far less likely to be casually read and
republished than one sitting in `~/.gitconfig`.

Each login gets its own entry named `<npub>/<8-char-alphanumeric-suffix>`;
the random suffix makes entries independent, so logging out of one
repository never breaks another login for the same account.

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

## Interop convention

A value of `nostr.nsec` / `nostr.bunker-app-key` matching
`npub1…/<8 alphanumeric chars>` is the name (the account/user field) of an
entry under keyring service `ngit`. The npub prefix is derived from the
stored secret itself and must be verified against the retrieved key on
read. Plaintext and `ncryptsec1…` values remain valid indefinitely;
applications without credential-store support can keep writing plaintext.

## Migration

When ngit reads a plaintext nsec or bunker app key from local or global git
config, it best-effort migrates it: the credential entry is written and
read-back-verified before the config value is replaced with the entry name,
and a notice is printed. Any failure leaves the plaintext in place and in
use, with a once-per-run warning. System-level git config is never
rewritten.

If you run ngit in a sandboxed or ephemeral environment (a container or
throwaway VM), migration moves the secret into *that environment's*
credential store; a git config shared with the host would then point at an
entry the host does not have. `ngit account export-keys` retrieves the
secret.

[`nostr-keyring`]: https://crates.io/crates/nostr-keyring
