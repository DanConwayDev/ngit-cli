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
logout. The default bunker (NIP-46) login for an identity uses
`signer:<user-npub>` and contains one typed, versioned record with the expected
user npub, sanitized bunker URI, and bunker client/app nsec. Additional bunker
connections for that same identity use `signer-alias:<alias>`. The one-time
`secret=` pairing parameter is removed before either record is saved. Entries
written by pre-release versions as `<npub>/<8-char-suffix>` are still read.

Platform keyrings cannot be enumerated portably, so ngit records the public
npub and alias of each successful credential write in
`<ngit-data-dir>/accounts.json`. This index never contains an nsec, bunker URI,
or app key. `ngit account whoami` revalidates every indexed identity through the
normal signer resolver before displaying it, and also scans `credentials.json`
and Git config so pre-index file-store, configured, and Git-config-only accounts
remain visible. A pre-index OS-keyring entry that is no longer referenced by
Git config is added to the index the next time it is selected by npub or alias;
the keyring itself still cannot reveal unknown entry names.

An unresolved inventory candidate does not prevent other usable accounts from
being listed. Human output warns on stderr with the candidate npub and lookup
diagnostic; JSON includes these entries in `unavailable_accounts` (each with
`npub` and `error`) alongside the usable `accounts`. This applies even with
`--signer`: whoami inventories all accounts. Explicit credential selection for
signing still fails if that credential is missing, invalid, or unavailable.

Git config remains the portable active-login selector:

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

The credential-store alias value remains a raw npub so older ngit versions can
continue reading it. New versions may also store a public
`alias-credential:<alias>` companion naming one concrete bunker credential.
This lets two aliases select different NIP-46 sessions for one npub without
exposing either session's secret. Older versions ignore the companion and fall
back to the identity's default signer. Git config likewise keeps only the
portable alias-to-npub mapping.

Aliases are resolved from the OS credential store first, then
`credentials.json`, then local, global, and system Git config. The JSON store
is the direct fallback for systems where the OS credential store is
unavailable. `nostr.signer = fred` (or an
npub) makes a signer the default for that Git-config scope. `ngit account login
--profile fred` writes the mapping to the selected Git-config scope and, unless
`git-config` secret storage was selected, the selected credential backend as
well. After `logout --keep-credentials` retains a stored signer, `ngit account login --local --profile
fred` reactivates it without requiring the nsec or bunker URL again.
`ngit account login --local -i --profile fred` instead starts a fresh interactive
login and assigns the resulting connection to `fred`, replacing that alias's
previous signer connection while leaving the bare-npub default intact.

A bare npub selects the identity's default credential when one exists. If the
default has been removed and exactly one alias-specific connection remains,
the npub and that alias both select the remaining connection. More than one
alias-specific connection without a default makes bare-npub selection
ambiguous, so an alias is required. Pairing a different remote-signer
connection without `--profile` replaces the default remote session for that
npub after authentication. Use `--profile NAME` to retain an additional
connection, or to replace that named profile's connection. Existing named
profiles keep their exact sessions when the default is replaced. An identity
key is never overwritten by this operation: when an nsec exists, use a named
profile for the additional remote signer. Replacement does not revoke the
old client remotely. Explicit `git-config` storage remains scope-specific.

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
`ngit account login <name>` resolves the name once and writes the resolved
npub to `nostr.signer`, exactly like npub reactivation, while a
one-shot `--signer <name>` re-resolves on every invocation and writes
nothing. The older `ngit account login --signer <name>` spelling remains
available.

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

When an alias binds a concrete bunker credential, ngit loads that credential
directly. Otherwise, after resolving an alias to its npub, ngit checks all nsec
sources before any bunker source. Within each type the order is OS credential
store, `credentials.json`, then matching local/global/system Git config.
Bunker fields are never assembled across scopes. The chosen nsec is checked by
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

`ngit account whoami` groups identities by npub, then lists each distinct
usable signer connection without exposing its secret. Each connection reports
whether it is a local key or remote-signer session, the npub and aliases that
select it, and its local/global/system and active scope badges. `ngit account
list` is an alias for the same inventory. The human footer defines `ACCOUNT`
once and shows how to use it for a single ngit or Git command, make it the
local or global default, add an alias, or remove a local override. An `ACCOUNT`
may be an available full npub, a listed alias, or the exact cached Nostr profile
name. Profile names shadowed by an alias or shared by multiple credentialed
accounts are marked unusable. A full npub is also unavailable when several
alias-specific sessions remain without a default, in which case `whoami`
requires an alias instead of advertising an ambiguous selector.

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

Commands such as `ngit init` select command-line credentials (`--signer` or
`--nsec`) first, then repository-local, global, and system Git configuration.
A configured local login that cannot be loaded causes an error rather than
falling back to a different global identity. Use `ngit account whoami --offline`
to inspect the active account and its scope.

`ngit account logout` removes the local login when one exists, otherwise the
global login. Use `--local` to clear only this repository's login (including
when it is already absent), or `--global` to clear only the global login while
preserving the local one. `--local` requires a Git repository. After removing a
local login, subsequent commands can use the remaining global login.

Ordinary logout removes the selected account's stored credentials and named
profile bindings from both credential backends. Other profiles are untouched,
even if they share the same npub. Other repositories selecting the deleted
profile lose access too; their selections remain and fail rather than silently
choosing another signer. Use `--keep-credentials` to explicitly retain credentials.
After removing a repository-specific login, ngit reports that this repository
now uses the default account selection, and lists its availability. A different
global account remains authenticated. Repeating bare logout after removing the
repository override targets the default account; use `--local` when only the
repository-specific login should be affected.

If logout would delete an identity nsec, it stops before making changes.
Choose `--forget` to delete identity keys as well, or
`--keep-credentials` to clear selections while preserving stored
credentials. An nbunksec is a remote session, not an identity nsec. Ordinary
logout also refuses to erase a plaintext identity key from Git config without
`--forget` or `--keep-credentials`; the latter saves the key into the credential
store first. Encrypted Git-config keys must be exported before removal because
logout cannot preserve them automatically without decrypting them.

Every logout lists saved profiles and remaining selections using the offline
account inventory (also returned in JSON). Backend or configuration cleanup
failures are errors rather than a claim of complete sign-out. OS credentials
are discoverable through ngit's public registry; arbitrary entries created by
other applications outside that registry cannot be enumerated portably.

To delete a retained credential after logout, or when its signer is offline:

```bash
ngit account logout --profile work --forget
ngit account logout <npub> --forget
ngit --signer work account logout --forget
```

Removal uses public selection metadata and does not contact the remote signer.
An npub selects its default credential; multiple additional profiles without a
default require `--profile NAME`. Deletion removes the selected credential and
its dependent profile bindings, cleans the account inventory, and preserves
other profiles. A public removal marker prevents old Git alias mappings in
other repositories from falling back to another connection; pairing the named
profile again clears that marker. No signing material is retained by it.
`--forget` deletes shared credentials machine-wide, even with
`--local`; other repositories pointing at that credential will need login.
It does not revoke permissions in the remote signer application.

`logout --forget` without an active login or explicit target fails with
instructions rather than silently doing nothing. `account whoami` prints a
removal command for each available signer. The low-level
`account forget-keys ENTRY` command remains available for direct recovery.

## Interop convention

A value of `nostr.nsec` / `nostr.bunker-app-key` that is a bare `npub1…`
(or the legacy `npub1…/<8 alphanumeric chars>` form) is the name (the
account/user field) of an entry under keyring service `nostr`. The npub is
derived from the stored secret itself and must be verified against the
retrieved key on read.

The service is `nostr`, not `ngit`, because these entries pair with `nostr.*`
git config keys and are useful to other nostr applications. Namespaces keep
record types distinct: bare `npub1…` for an identity nsec, `signer:npub1…` for
the default typed bunker record, `signer-alias:<name>` for an additional bunker
connection, `alias:<name>` for the raw npub alias mapping, and
`alias-credential:<name>` for its optional public credential binding.

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

Established NIP-46 connections have the equivalent `--nbunksec VALUE` and
`--nbunksec-file PATH` forms. An `nbunksec` bundles the remote-signer pubkey,
client/app secret key, relays, and optional original pairing secret using the
Applesauce/nsyte TLV encoding. It does not contain the user's npub, so a
one-shot command asks the remote signer for `get_public_key`. Logging in with
`ngit account login --nbunksec-file PATH` associates that result with the
stored connection; later commands use the stored npub and validate every
returned event against it.

A fresh clone consults an already-decrypted cached private relay list alongside
the URL's relay hint. This initial lookup does not ask the signer to decrypt.
The plaintext must match the newest list event in the account's event cache;
an older cached list is not reused when a newer list is known.
An explicitly public clone (`--config nostr.private=false`) skips these private
locations. Once an announcement is found, an unrelated cached private relay's
failure does not block the clone or cause discovery to expand to public indexers.
ngit then fetches state and proposals from the relays named in the announcement.
An explicitly supplied URL hint may request relay authentication from the
configured signer during discovery.

If the announcement is still missing and the repository is known private,
ngit refreshes the account's private relay list and decrypts it if necessary,
then retries discovery on those relays. A GRASP-08 advertisement establishes
privacy automatically. For a private URL without a relay hint or cached
announcement, `git clone --config nostr.private=true <url>` supplies that
classification. Missing public or unclassified announcements never trigger
decryption, and known-private discovery never falls back to public indexers.

`ngit account export-keys` exports the matching portable credential for the
selected account: `npub` plus `nsec` for a local-key account, or `npub` plus
`nbunksec` for a remote-signer account. The `nbunksec` is an established
connection credential, not the remote user's identity secret. The default
human mode offers print and QR-code choices; `--secret` prints only the nsec or
nbunksec for shell use, while `--json` returns the typed secret field alongside
the npub. Use `ngit --signer <account> account export-keys` to select a
non-default stored account.

[`keyring`]: https://crates.io/crates/keyring

### Named signer profiles

`account login --profile NAME` names or reactivates a signer profile. The old
`--alias` spelling remains accepted for scripts. Existing alias names and
credential bindings are unchanged; no migration or re-pairing is needed.
A signer profile is distinct from the account’s public Nostr display name.
The stored Git-config keys and JSON `aliases` fields retain their existing
names for compatibility.

Login authenticates and stores the new signer before updating its selected
scope. Failed authentication or credential storage leaves the previous login
selection intact. A global login preserves repository-local overrides; use
`account logout --local` to inherit the global selection in that repository.
