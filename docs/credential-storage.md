# Credential storage

How ngit stores nostr secrets, and the git-config convention other nostr
applications can rely on.

## Where secrets live

`ngit account login` stores the secret in the OS credential store (macOS
Keychain, Windows Credential Manager, or the D-Bus secret service on Linux)
via [`nostr-keyring`], under keyring **service `ngit`**. Each login gets its
own entry named `<npub>/<8-char-alphanumeric-suffix>`; the random suffix makes
entries independent, so logging out of one repository never breaks another
login for the same account.

Git config remains the index, because platform keyrings cannot be enumerated:

| git config key         | value                                                    |
| ---------------------- | -------------------------------------------------------- |
| `nostr.nsec`           | keyring entry name, plaintext `nsec1…`, or `ncryptsec1…` |
| `nostr.bunker-app-key` | keyring entry name or plaintext key                      |

## Interop convention

A value of `nostr.nsec` / `nostr.bunker-app-key` matching
`npub1…/<8 alphanumeric chars>` is the name (the account/user field) of an
entry under keyring service `ngit`. The npub prefix is derived from the stored
secret itself and must be verified against the retrieved key on read.
Plaintext and `ncryptsec1…` values remain valid indefinitely; applications
without credential-store support can keep writing plaintext, which ngit
migrates on read where possible.

## Migration

When ngit reads a plaintext nsec or bunker app key from local or global git
config, it best-effort migrates it: the keyring entry is written and
read-back-verified before the config value is replaced with the entry name,
and a notice is printed. Any failure leaves the plaintext in place and in use,
with a once-per-run warning. System-level git config is never rewritten.

If you run ngit in a sandboxed or ephemeral environment (a container or
throwaway VM), migration moves the secret into *that environment's* credential
store; a git config shared with the host would then point at an entry the host
does not have. `ngit account export-keys` retrieves the secret.

## Opting out

Set `nostr.credential-store` to `false` in git config (or export
`NGIT_CREDENTIAL_STORE=false`) to disable keyring writes and migration.
Existing pointer values are still read. Headless systems without a usable
credential store fall back to plaintext-in-git-config automatically after a
warning.

[`nostr-keyring`]: https://crates.io/crates/nostr-keyring
