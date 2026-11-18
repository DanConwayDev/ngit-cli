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
Each JSON read/modify/replace holds an OS lock on a separate `.lock` file,
including public-index updates. This prevents independent ngit processes from
losing each other's entries. Locks release automatically when the owning process
exits; acquisition has a 30-second deadline with retry guidance. The empty lock
files may remain after logout. This is not a cross-store transaction, and older
ngit versions or external editors that ignore the lock cannot participate in it.

On Unix systems using D-Bus, an unset `DBUS_SESSION_BUS_ADDRESS` and a missing
session-bus socket (`$XDG_RUNTIME_DIR/bus`, or `/run/user/<uid>/bus`) mean no
OS backend is configured in that environment. A running bus that reports no
running or activatable Secret Service is also an absent backend. File-backed
credentials can be removed there, including named profiles. Deletion reports that only file-store
entries are being removed. This does not delete credentials in
an OS keyring belonging to another desktop session. A configured but unreachable
bus, a locked keyring, and backend initialization failures remain errors;
`NGIT_SECRET_STORAGE=file` does not authorize ignoring those errors on deletion.
Deletion checks OS access before changing selected credentials or removal
markers. If an OS deletion fails before file deletion, new removal markers are
rolled back. Multiple backend operations are not a cross-store transaction;
failures after a successful deletion can still require cleanup and are reported.

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

When an OS store is inaccessible, its public account index still protects
known profile names from reassignment to a different identity. Unlock the
store and remove the old profile, or use a new name. Otherwise an older OS
mapping could silently take precedence again after recovery. New profile
names and reusing the same identity retain file fallback support.

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

To save a profile for explicit use without selecting it locally or globally:

```bash
ngit account login -i --profile snode-ci --no-activate
git -c nostr.signer=snode-ci push
```

`--no-activate` also works with `--nostrconnect`, `--bunker-url`, imported keys
or connections, and existing account selectors. It requires `--profile`,
conflicts with `--local`, and uses `auto` or `file` credential storage rather
than Git config. No Git-config selection or alias mapping is written. On a
machine without an OS credential store, `auto` saves the profile and connection
in `credentials.json`. Replacing a profile still changes the connection used
by commands that already select that profile.

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

When interactive login finds an unnamed login, it offers a simple sign-in or
cancel choice. When named profiles are involved, it names the global default or
repository login and identifies the existing saved profile that will be kept.
Choosing to keep that selection cancels the login before authentication; it does not save a new profile. To save and
select a newly paired profile, choose to make it the default (or repository
override). Other credential-store profiles remain saved. Pairing over an existing
named profile can replace that profile's signer connection, so the prompt warns
about that separately. These choices do not invoke logout or delete other saved
profiles.

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

## Local signer socket

ngit can sign without holding any secret at all. When a signer daemon listens
on a Unix socket speaking the draft NIP-5F protocol
([nostr-protocol/nips#1862](https://github.com/nostr-protocol/nips/pull/1862)),
such as [nostrd] or Amber Desktop, ngit asks it for signatures and NIP-44
encryption instead of reading a key or opening a bunker connection.

ngit looks for the socket at `~/.local/share/nostr/signer.sock`, where NIP-5F
clients look, or at the path in the `nostr.signer-socket` Git-config item.
Setting `nostr.signer-socket` to `false` turns the socket off.

An npub selects an identity; a named profile selects a saved backend.
`--signer <npub>` and `nostr.signer=<npub>` prefer the socket for that exact
account, including non-active accounts. They use ngit's stored credentials
only when the socket is absent/disabled or reports "account not found".
Locked signers and other socket errors fail without changing backend.
Explicit nsec/bunker credentials and ordinary profiles retain their existing
backend, even when the socket holds the same identity.

With no login selected, the daemon's active account is the fallback. Saved
but inactive profiles do not prevent this.

```sh
ngit --signer <npub> issue create --subject "Bug" --body "Details"
git -c nostr.signer=<npub> push origin pr/topic
# Pin the daemon's current identity and the socket backend:
ngit account login --socket --profile desktop
# Or save a particular identity without activating it:
ngit account login <npub> --socket --profile desktop --no-activate
ngit account login --local desktop
ngit --signer desktop issue create --subject "Bug" --body "Details"
```

A socket profile stores only an npub and a public backend binding, using the
existing profile store in auto/file mode. It requires the socket to hold that
identity and never falls back to local keys or a bunker. Saving a profile
from an npub that resolves through the socket also binds that backend;
`--socket` explicitly requires it when creating the profile. The name
`socket` is not reserved and has no special selection behavior.

Each command pins the public key for all requests. Changing the daemon's
active account affects automatic fallback on the next command, but cannot
change a profile's pinned identity or an operation already running.

`account whoami` reports availability and the effective socket account;
JSON exposes this separately in `socket`. Inventory probes are bounded and
do not sign anything. A socket file existing is not proof of readiness.

Logout removes ngit's selected profile and its binding as usual, then reports
any socket fallback. It does not delete the external account's keys or revoke
signer permissions. With only automatic socket signing active, logout errors
with instructions to manage the account in the signer application or disable
socket signing. `logout --all` cannot revoke the external signer either.
`export-keys` cannot export a socket account's secrets.

```sh
git config --global nostr.signer-socket false
# A repository setting overrides the global setting:
git config nostr.signer-socket false
```

The protocol remains a draft; its wire format may change before adoption.

[nostrd]: https://git.shakespeare.diy/npub1q3sle0kvfsehgsuexttt3ugjd8xdklxfwwkh559wxckmzddywnws6cd26p/nostrd.git

## Choosing where secrets live

The `nostr.secret-storage` git config item — overridden by the
`NGIT_SECRET_STORAGE` environment variable, or per invocation by
`ngit account login --secret-storage <value>` — selects the policy:

- `auto` (default): OS credential store, falling back to the file store.
- `file`: write to ngit's file store, never the OS store. If different data
  already exists under the same higher-priority OS entry, login fails with a
  removal command instead of writing a credential that could never be used.
- `git-config`: plaintext in git config, as ngit stored secrets previously.

When a secret cannot be stored under `auto` or `file`, named-profile login
fails without changing the configured login. It never substitutes a binding
to the identity's default signer for an unsaved connection. Login without a
profile can explicitly offer a plaintext Git-config fallback interactively.

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

Use `ngit account logout --all` to remove all known saved profiles and remote
sessions from both credential backends. It clears the current repository's
login and non-local Git login configuration (user, XDG and enabled system
files), including referenced files even when their include conditions are
inactive here. It respects Git's config environment overrides. It never
searches for other repositories: their local plaintext credentials and
selections may remain. Shared stored sessions removed by `--all` become
unavailable to those repositories.

If logout would delete an identity nsec, it returns an error before making
changes. There is no interactive confirmation, including with `-i`.
Choose `--forget` to delete identity keys as well, or
`--keep-credentials` to clear selections while preserving stored
credentials. An nbunksec is a remote session, not an identity nsec. Ordinary
logout also refuses to erase a plaintext identity key from Git config without
`--forget` or `--keep-credentials`; the latter saves the key into the credential
store first. Encrypted Git-config keys must be exported before removal because
logout cannot preserve them automatically without decrypting them. Likewise,
`--keep-credentials` refuses to clear legacy remote signer configuration in Git
config, since retaining only its app key would lose the connection details.
Export that connection before removing its local configuration. These failures
happen before changes, for both ordinary logout and `--all`.

Every logout lists saved profiles and remaining selections using the offline
account inventory (also returned in JSON). JSON `unavailable_logins` entries identify a scope
and error when a remaining selection cannot be used; they do not activate
a lower-priority account. Backend or configuration cleanup
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
An explicitly supplied npub removes all known stored credentials and profile
bindings for that identity: its identity key, default remote session, named
sessions, and known legacy key entries. Identity keys still require `--forget`.
Use `--profile NAME` to remove just one profile instead. Ordinary logout without
an explicit account continues to remove only the selected connection.
Deletion cleans the account inventory and preserves other identities. A public removal marker prevents old Git alias mappings in
other repositories from falling back to another connection; pairing the named
profile again clears that marker. No signing material is retained by it.
`--forget` deletes shared credentials machine-wide, even with
`--local`; other repositories pointing at that credential will need login.
It does not revoke permissions in the remote signer application.

`logout --forget` without an active login or explicit target fails with
instructions rather than silently doing nothing. `account whoami` leads with the
active login and any repository override,
then lists saved accounts and signer connections. Its concise command guide
distinguishes removing one named profile from removing an entire npub; identity
key deletion is never suggested with an automatic `--forget`. The low-level
`account forget-keys ENTRY` command remains available for direct recovery.
`account forget-keys <npub>` explicitly deletes all known stored credentials for
that identity, including its identity key, without another confirmation.
`account forget-keys signer:<npub>` or `signer-alias:<name>` deletes only that
exact entry and its dependent bindings. These low-level commands leave Git login
selections in place; `account logout <npub> --forget` also clears matching login
selections in the applicable scopes. Neither searches other repositories for
independent plaintext credentials.

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

## Relay authentication

By default, ngit answers a NIP-42 AUTH challenge only after the relay rejects
an actual read or write with `auth-required:`. After successful authentication,
the SDK resends the subscription or retries the same signed publication.
Public reads avoid unnecessary authentication signatures.

For a relay that silently withholds data until authentication, enable
proactive authentication for that operation:

```bash
ngit --proactive-auth repo --json
git -c nostr.proactive-auth=true pull
```

`nostr.proactive-auth` defaults to `false`. Set it with `git config` for a
repository or add `--global` for all repositories. `NGIT_PROACTIVE_AUTH=true`
(or `1`) overrides Git config; `NGIT_PROACTIVE_AUTH=false` disables proactive
authentication even when Git config enables it. An explicit `--proactive-auth`
flag takes precedence over the environment for ngit and its child processes.

This changes authentication timing only. An eligible repository or publishing
relay and an already attached signer are still required; a challenge never
activates a signer. Known-private repositories still require a selected
account. GRASP-08's Git HTTP authentication remains separate from NIP-42.

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

`account login --profile NAME` names or reactivates a signer profile.
Reactivating a saved profile selects its existing connection; it does not pair
again or change its relays. To replace the connection while keeping the current
default, use `account login --nostrconnect --profile NAME --no-activate
--signer-relay URL`. `--signer-relay` is rejected outside new nostrconnect
pairing (including the interactive pairing flow). A `--bunker-url` uses the
relays supplied in that URL. The old
`--alias` spelling remains accepted for scripts. Existing alias names and
credential bindings are unchanged; no migration or re-pairing is needed.
A signer profile is distinct from the account’s public Nostr display name.
The stored Git-config keys and JSON `aliases` fields retain their existing
names for compatibility.

Login authenticates and stores the new signer before updating its selected
scope. Failed authentication or credential storage leaves the previous login
selection intact. A global login preserves repository-local overrides; use
`account logout --local` removes the repository override and its credentials,
then inherits the global selection. Add `--keep-credentials` to retain the
connection for reuse. Shared credentials may also be used by other repositories.

`account logout --all --keep-credentials` does not need to read saved secrets
to clear login selections. It works while the OS store is locked; the final
inventory reports accounts that cannot currently be checked. Legacy plaintext
keys still require successful preservation before their Git configuration is
removed, and legacy remote connections still require export first.
